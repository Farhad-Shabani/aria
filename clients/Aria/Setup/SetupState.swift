import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupState")

/// The setup wizard, step by step. Decisions live here; the OS effects are
/// injected so tests can walk every step without a daemon.
@MainActor
final class SetupState: ObservableObject {
    struct Effects {
        var inspect: () async throws -> Aria_Setup_V1_Inspection
        var apply:
            (Aria_Setup_V1_ApplyRequest, @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void) async throws
                -> Aria_Setup_V1_Summary
        /// Requests a stop before the service takes the socket; waits only
        /// for a bounded time.
        var stopDaemon: () async -> Void
        var startDaemon: () async -> Void
        var ariaKeyState: () async -> AriaKeyState
        var openPrivacySettings: (AriaKeyState) -> Void
        var speakers: () async throws -> [EnrolledSpeaker]
        var enroll: (String) async throws -> Void
        /// Returns what to tell the user when launch at login could not be enabled.
        var launchAtLogin: () -> String?
        /// Asks for an administrator's password. Returns what to tell the
        /// user when the command did not land.
        var installCommandLineTool: () -> String?
        var waitForGrant: () async throws -> Void = { try await Task.sleep(for: .seconds(1)) }
        var now: () -> Date = Date.init
    }

    enum Step: Hashable {
        case inspecting
        case welcome
        case choices
        case applying
        case grant
        case voice
        case tryOut
        case openAria
        case done
        case failed(String)
    }

    struct InstallRow: Equatable {
        enum Progress: Equatable {
            case pending
            case running
            case done
        }

        let step: Aria_Setup_V1_Step
        let title: String
        let progress: Progress
    }

    private static let installSteps: [(step: Aria_Setup_V1_Step, title: String)] = [
        (.settings, "Settings"), (.models, "Models"), (.service, "Background service"),
    ]

    @Published private(set) var step: Step = .inspecting {
        didSet {
            isMovingForward = step.order >= oldValue.order
            progress.record(step)
            if step == .inspecting { forgetLastWalk() }
            // Access granted after Skip on Allow Access still reaches Try It and Open Aria.
            if step == .tryOut, !isHotkeyReady { watchGrant() }
        }
    }
    @Published private(set) var progress = SetupProgress()
    /// Whether the last step change moved ahead, so pages slide the right way.
    @Published private(set) var isMovingForward = true
    @Published private(set) var inspection = Aria_Setup_V1_Inspection()
    /// New engines bring a new model list, so the download picks start over.
    @Published var selections = Aria_Setup_V1_Selections() {
        didSet {
            if step == .choices { downloads = defaultDownloads }
        }
    }
    @Published var downloads = Set<String>()
    @Published private(set) var applying: Aria_Setup_V1_Step = .unspecified
    @Published private(set) var downloadProgress: [String: Aria_Setup_V1_DownloadProgress] = [:]
    /// What the running install fetches, so its bar counts models not started yet.
    private var fetching: [Aria_Setup_V1_Model] = []
    private var downloadPace = DownloadPace()
    @Published private(set) var downloadTimeLeft: String?
    private var summary: Aria_Setup_V1_Summary?
    @Published var voiceName = ""
    /// Nil until the daemon starts recording the voice.
    @Published private(set) var voiceSampling: VoiceSampling?
    @Published private(set) var enrolled: String?
    /// What the Aria key typed into Try It's field.
    @Published var typed = ""
    /// What the last Aria key dictation typed during Try It, which stays on
    /// the page so the person sees their words land.
    @Published private(set) var typedWordCount: Int?
    @Published private(set) var ariaKeyState = AriaKeyState.needsAccessibility
    /// Whether the dictation hotkey answers, so Done can teach it.
    var isHotkeyReady: Bool { ariaKeyState == .armed }
    /// A step's own failure, which setup carries on past.
    @Published private(set) var notice: String?
    /// What install couldn't set up, which the last page names.
    @Published private(set) var installNotice: String?
    @Published private(set) var isBusy = false

    private let effects: Effects
    private let runner: SetupRunner
    private var applyTask: Task<Void, Never>?
    private var grantTask: Task<Void, Never>?

    init(effects: Effects, runner: SetupRunner) {
        self.effects = effects
        self.runner = runner
    }

    /// The models the chosen engines want, plus the ones every install
    /// wants, whether on disk or not. While the server recognizes, the
    /// local engine's models wait until recognition comes back.
    static func wanted(_ inspection: Aria_Setup_V1_Inspection, _ selections: Aria_Setup_V1_Selections)
        -> [Aria_Setup_V1_Model]
    {
        let recognizesHere = inspection.sttRunsOn != .server
        return inspection.models.filter { model in
            model.engineID.isEmpty
                || (recognizesHere && model.engineID == selections.sttEngineID)
                || model.engineID == selections.ttsEngineID
        }
    }

    /// The wanted models that setup still has to fetch.
    static func plan(_ inspection: Aria_Setup_V1_Inspection, _ selections: Aria_Setup_V1_Selections)
        -> [Aria_Setup_V1_Model]
    {
        wanted(inspection, selections).filter { $0.files == .partial || $0.files == .absent }
    }

    static func expressPlan(_ inspection: Aria_Setup_V1_Inspection, _ selections: Aria_Setup_V1_Selections)
        -> [Aria_Setup_V1_Model]
    {
        plan(inspection, selections).filter(\.downloadByDefault)
    }

    static func total(_ models: [Aria_Setup_V1_Model]) -> String {
        models.isEmpty ? "No downloads needed." : "Total download: \(downloadSize(of: models))"
    }

    /// Says "up to" when a download resumes: only the transfer knows how
    /// much is left.
    nonisolated static func downloadSize(of models: [Aria_Setup_V1_Model]) -> String {
        let size = size(models.reduce(0) { $0 + $1.sizeBytes })
        return models.contains { $0.files == .partial } ? "up to \(size)" : size
    }

    /// What picking `engine` downloads, for comparing engines before one
    /// is chosen.
    nonisolated static func download(
        of engine: Aria_Setup_V1_Choice, in inspection: Aria_Setup_V1_Inspection
    ) -> EngineDownload {
        switch engine.modelSource {
        case .downloads: break
        case .fetchedOnFirstUse: return .status("Fetched on first use")
        case .builtIn: return .status("Built in")
        case .unspecified, .UNRECOGNIZED: return .status("Size unknown")
        }
        let models = inspection.models.filter { $0.engineID == engine.id }
        if models.contains(where: { $0.files == .external }) { return .status("Custom model") }
        let missing = models.filter { $0.files == .partial || $0.files == .absent }
        if missing.isEmpty { return .status("Downloaded") }
        return .size(downloadSize(of: missing))
    }

    /// Why the choices cannot be applied: an engine this Mac cannot run.
    private static func blocked(_ selections: Aria_Setup_V1_Selections, in inspection: Aria_Setup_V1_Inspection)
        -> String?
    {
        let chosen = [
            (selections.sttEngineID, inspection.sttChoices),
            (selections.ttsEngineID, inspection.ttsChoices),
        ]
        for (key, engines) in chosen {
            guard let engine = engines.first(where: { $0.id == key }) else { continue }
            if engine.availability == .unsupported { return "\(engine.label): \(engine.availabilityDetail)" }
        }
        return nil
    }

    /// The wizard opens itself once, and only while something is missing. A
    /// speech model setup still has to download reopens it, because the
    /// daemon can't start without one.
    static func shouldOffer(_ inspection: Aria_Setup_V1_Inspection, offeredBefore: Bool) -> Bool {
        guard !wizardWork(in: inspection).isEmpty else { return false }
        return !offeredBefore || SetupStatus.isMissingSttModel(in: inspection)
    }

    /// What the wizard can put in place. A remapped Aria key shows only in
    /// Finish Setting Up.
    static func wizardWork(in inspection: Aria_Setup_V1_Inspection) -> [Aria_Setup_V1_Missing] {
        inspection.missing.filter { missing in
            switch missing.kind {
            case .keyRemapped: false
            case .settings, .wakeword, .model, .service, .grant, .unspecified, .UNRECOGNIZED: true
            }
        }
    }

    /// A voice can be recorded once its model is on disk and nobody is
    /// enrolled yet.
    static func offersVoice(_ summary: Aria_Setup_V1_Summary, speakers: [EnrolledSpeaker]) -> Bool {
        speakers.isEmpty && summary.voiceModelPresent
    }

    var blockedBy: String? { Self.blocked(selections, in: inspection) }

    /// What the wizard still has to do on this Mac, as one line; nothing
    /// until the Mac has been looked at.
    var missingSummary: String? {
        guard step != .inspecting else { return nil }
        let names = Self.wizardWork(in: inspection).map { missing in
            switch missing.kind {
            case .settings: "settings"
            case .wakeword: "wake-word models"
            case .model: "models"
            case .service: "background service"
            case .grant: "Aria key permission"
            case .keyRemapped, .unspecified, .UNRECOGNIZED: "unknown"
            }
        }
        var seen = Set<String>()
        let unique = names.filter { seen.insert($0).inserted }
        return unique.isEmpty ? "Everything is in place." : "Missing: \(unique.joined(separator: ", "))."
    }

    /// What Continue downloads. Nil when it downloads nothing, which the
    /// missing summary already implies.
    var expressTotal: String? {
        let plan = Self.expressPlan(inspection, selections)
        return plan.isEmpty ? nil : Self.total(plan)
    }

    var downloadTotal: String {
        let plan = Self.plan(inspection, selections)
        let chosen = plan.filter { downloads.contains($0.id) }
        return chosen.isEmpty && !plan.isEmpty ? "Nothing selected." : Self.total(chosen)
    }

    /// One note per chosen engine that downloads its own model, which
    /// setup cannot size.
    var selfFetchingNotes: [String] {
        let chosen = [
            (selections.sttEngineID, inspection.sttChoices),
            (selections.ttsEngineID, inspection.ttsChoices),
        ]
        return chosen.compactMap { key, engines in
            engines.first { $0.id == key && $0.modelSource == .fetchedOnFirstUse }
                .map { "\($0.label) downloads its model on first use." }
        }
    }

    var installRows: [InstallRow] {
        let current = Self.installSteps.firstIndex { $0.step == applying }
        return Self.installSteps.enumerated().map { index, entry in
            let progress: InstallRow.Progress =
                switch current {
                case .some(let running) where index < running: .done
                case .some(let running) where index == running: .running
                default: .pending
                }
            return InstallRow(step: entry.step, title: entry.title, progress: progress)
        }
    }

    /// Every model the install fetches, as one transfer. A model not started
    /// yet counts at the size setup lists for it.
    var downloadTransfer: ModelTransfer? {
        guard !fetching.isEmpty else { return nil }
        var received: UInt64 = 0
        var total: UInt64 = 0
        for model in fetching {
            let transfer = downloadProgress[model.id].flatMap(ModelTransfer.init)
            let size = transfer?.total ?? model.sizeBytes
            total += size
            received += min(transfer?.received ?? 0, size)
        }
        return ModelTransfer(received: received, total: total)
    }

    func title(ofModel id: String) -> String {
        inspection.models.first { $0.id == id }?.title ?? id
    }

    nonisolated static func size(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }

    func start() {
        switch step {
        case .inspecting, .welcome, .done, .failed:
            step = .inspecting
            Task { await inspectNow() }
        case .grant:
            watchGrant()
        case .tryOut where !isHotkeyReady:
            watchGrant()
        default:
            return
        }
    }

    /// Closing the window on the last page ends setup, so reopening starts a new walk.
    func stop() {
        grantTask?.cancel()
        if step == .openAria { step = .done }
    }

    /// Inspecting begins every walk, so nothing an earlier walk did carries over.
    private func forgetLastWalk() {
        summary = nil
        installNotice = nil
        voiceName = ""
        enrolled = nil
        typed = ""
        typedWordCount = nil
        notice = nil
    }

    func express() {
        guard step == .welcome, blockedBy == nil else { return }
        downloads = defaultDownloads
        runApply()
    }

    func next() {
        switch step {
        case .welcome:
            downloads = defaultDownloads
            step = .choices
        case .choices:
            guard blockedBy == nil else { return }
            runApply()
        case .grant:
            grantTask?.cancel()
            Task { await enterVoice() }
        case .voice:
            step = .tryOut
        case .tryOut:
            step = .openAria
        case .openAria:
            step = .done
        default:
            break
        }
    }

    func back() {
        switch step {
        case .choices: step = .welcome
        default: break
        }
    }

    private var defaultDownloads: Set<String> {
        Set(Self.expressPlan(inspection, selections).map(\.id))
    }

    func cancel() {
        applyTask?.cancel()
    }

    func retry() {
        guard case .failed = step else { return }
        step = .inspecting
        Task { await inspectNow() }
    }

    func openPrivacySettings() {
        effects.openPrivacySettings(ariaKeyState)
    }

    func recordVoice() {
        let name = voiceName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, !isBusy else { return }
        isBusy = true
        notice = nil
        voiceSampling = VoiceSampling()
        Task {
            do {
                try await effects.enroll(name)
                enrolled = "Voice saved for \(name)."
                step = .tryOut
            } catch {
                notice = Self.notice(for: error, during: "enroll")
            }
            isBusy = false
            voiceSampling = nil
        }
    }

    func speakerEvent(_ event: Aria_Daemon_V1_SpeakerEvent) {
        guard step == .voice, isBusy else { return }
        voiceSampling?.hear(event)
    }

    /// Try It counts only an Aria key dictation that typed words, here or anywhere.
    func dictationEvent(_ event: Aria_Daemon_V1_DictationEvent) {
        guard step == .tryOut, event.stage == .delivered, event.outcome == .inserted, event.wordCount > 0
        else { return }
        typedWordCount = Int(event.wordCount)
    }

    /// Setup ends once the shortcut it teaches opened Aria.
    func shortcutOpenedAria() {
        guard step == .openAria else { return }
        step = .done
    }

    private func inspectNow() async {
        do {
            let inspection = try await effects.inspect()
            self.inspection = inspection
            selections = inspection.defaults
            notice = nil
            step = .welcome
        } catch {
            step = .failed(Self.message(for: error, during: "inspect", otherwise: Self.inspectFailure))
        }
    }

    private func runApply() {
        let origin = step
        step = .applying
        applying = .unspecified
        downloadProgress = [:]
        fetching = Self.plan(inspection, selections).filter { downloads.contains($0.id) }
        downloadPace = DownloadPace()
        downloadTimeLeft = nil
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = selections
        request.downloads = fetching.map(\.id)
        applyTask = Task {
            do {
                let summary = try await runner.run {
                    try await SetupClient.handingOffDaemon(
                        inspection, stop: effects.stopDaemon, start: effects.startDaemon
                    ) {
                        try await effects.apply(request) { event in
                            Task { @MainActor in self.absorb(event) }
                        }
                    }
                }
                await finishApply(summary)
            } catch SetupClientError.cancelled {
                step = origin
            } catch {
                step = .failed(Self.message(for: error, during: "apply", otherwise: Self.applyFailure))
            }
        }
    }

    private func absorb(_ event: Aria_Setup_V1_SetupEvent) {
        switch event.event {
        // Bundled wake-word models are copied in a blink; the app shows no step for it.
        case .started(.wakeword): break
        case .started(let step): applying = step
        case .download(let download):
            downloadProgress[download.modelID] = download
            guard let transfer = downloadTransfer else { return }
            downloadPace.record(transfer.received, at: effects.now())
            downloadTimeLeft = downloadPace.timeLeft(of: transfer.total)
        case nil: break
        }
    }

    /// Login and the `aria` command finish the install while the person still
    /// watches it, so the password prompt never lands over another app.
    private func finishApply(_ summary: Aria_Setup_V1_Summary) async {
        self.summary = summary
        let problems = [effects.launchAtLogin(), effects.installCommandLineTool()].compactMap { $0 }
        installNotice = problems.isEmpty ? nil : problems.joined(separator: " ")
        ariaKeyState = summary.grant == .armed ? .armed : .needsAccessibility
        if summary.grant != .armed {
            step = .grant
            watchGrant()
        } else {
            await enterVoice()
        }
    }

    private func watchGrant() {
        grantTask?.cancel()
        grantTask = Task {
            while !Task.isCancelled {
                let state = await effects.ariaKeyState()
                // Skip may have moved on while the check was out.
                guard !Task.isCancelled else { return }
                ariaKeyState = state
                if state == .armed {
                    if step == .grant { await enterVoice() }
                    return
                }
                try? await effects.waitForGrant()
            }
        }
    }

    private func enterVoice() async {
        guard let summary else { return }
        do {
            let speakers = try await effects.speakers()
            step = Self.offersVoice(summary, speakers: speakers) ? .voice : .tryOut
        } catch {
            notice = Self.notice(for: error, during: "speaker load")
            step = .tryOut
        }
    }

    static let inspectFailure = "Aria couldn’t check your Mac. Try again, or run `aria doctor` in Terminal to see why."
    static let applyFailure = "Setup couldn’t finish. Try again, or run `aria doctor` in Terminal to see why."
    static let missingBinary = "Aria’s installation is incomplete. Reinstall Aria, then try again."
    static let unknownFailure = "Something went wrong. Try again."

    /// Speaker errors carry text written for the user. Any other
    /// error's text goes to the log instead.
    private static func message(
        for error: any Error, during step: String, otherwise fallback: String = unknownFailure
    ) -> String {
        if error is SpeakerFailure { return error.localizedDescription }
        log.error("Setup \(step, privacy: .public) failed: \(String(describing: error), privacy: .private)")
        if case .ariaBinaryMissing? = error as? DaemonProcessError { return missingBinary }
        return SetupClientError.message(for: error, otherwise: fallback)
    }

    /// Nil for a cancellation, which needs no notice.
    private static func notice(
        for error: any Error, during step: String, otherwise fallback: String = unknownFailure
    ) -> String? {
        error is CancellationError ? nil : message(for: error, during: step, otherwise: fallback)
    }
}

/// A size to compare engines by, or why an engine has none.
enum EngineDownload: Equatable {
    case size(String)
    case status(String)

    /// Short enough for a menu row.
    var brief: String {
        switch self {
        case .size(let text), .status(let text): text
        }
    }

    /// Under the picker, which already shows the brief form: only a bare
    /// size needs its label spelled out.
    var note: String? {
        switch self {
        case .size(let text): "Download size: \(text)"
        case .status: nil
        }
    }
}

extension SetupState.Step {
    var stage: SetupProgress.Stage? {
        switch self {
        case .inspecting, .welcome: .welcome
        case .choices: .engines
        case .applying: .install
        case .grant: .hotkey
        case .voice: .voice
        case .tryOut: .tryOut
        case .openAria: .openAria
        case .done, .failed: nil
        }
    }

    /// Where the step sits in the walk, so pages slide the right way.
    /// Inspecting and welcome share a page, so they share a slot. Done
    /// closes the window over the last page, so it keeps that page's slot.
    var order: Int {
        if let stage { return stage.rawValue }
        if self == .done { return SetupProgress.Stage.openAria.rawValue }
        return SetupProgress.Stage.allCases.count
    }

    var title: String {
        switch self {
        case .inspecting, .welcome: "Welcome to Aria"
        case .choices: "Choose Your Engines"
        case .applying: "Installing…"
        case .grant: "Allow Access"
        case .voice: "Set Up Your Voice"
        case .tryOut: "Try Dictation"
        case .openAria, .done: "Open Aria from Any App"
        case .failed: "Setup Didn’t Finish"
        }
    }

    var caption: String? {
        switch self {
        case .inspecting, .welcome: "Dictation stays on your Mac. Chat uses your selected assistant."
        case .choices: "Speech stays on your Mac. You can change these later."
        case .applying: "This takes a moment. Downloads can be canceled."
        case .grant: "Aria needs permission to hear the Aria key and type for you."
        case .voice: "Aria can tell who is speaking once it knows your voice."
        case .tryOut: "Tap the Aria key and say a few words."
        case .openAria, .done: "Try the shortcut now, or click **Done**."
        case .failed: nil
        }
    }
}
