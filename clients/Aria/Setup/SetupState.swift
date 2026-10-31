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
        var hotkeyArmed: () async -> Bool
        var openAccessibility: () -> Void
        var speakers: () async throws -> [Aria_Daemon_V1_EnrolledSpeaker]
        var enroll: (String) async throws -> Aria_Daemon_V1_EnrollSpeakerResponse
        var transcribe: () async throws -> String
        var speak: (String) async throws -> Void
        /// Returns what to tell the user when launch at login could not be enabled.
        var launchAtLogin: () -> String?
        /// Asks for an administrator's password. Returns what to tell the
        /// user when the command did not land.
        var installCommandLineTool: () -> String?
        var waitForGrant: () async throws -> Void = { try await Task.sleep(for: .seconds(1)) }
    }

    enum Step: Hashable {
        case inspecting
        case welcome
        case choices
        case applying
        case grant
        case voice
        case tryOut
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

    static let spokenReply = "Aria is ready."
    private static let installSteps: [(step: Aria_Setup_V1_Step, title: String)] = [
        (.settings, "Settings"), (.models, "Models"), (.service, "Background service"),
    ]

    @Published private(set) var step: Step = .inspecting {
        didSet {
            isMovingForward = step.order >= oldValue.order
            progress.record(step)
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
    @Published private(set) var summary: Aria_Setup_V1_Summary?
    @Published var voiceName = ""
    @Published private(set) var voicePrompt: String?
    @Published private(set) var enrolled: String?
    @Published private(set) var heard: String?
    /// Whether the dictation hotkey answers, so Done can teach it.
    @Published private(set) var isHotkeyReady = false
    /// A step's own failure, which setup carries on past.
    @Published private(set) var notice: String?
    @Published private(set) var isBusy = false

    private let effects: Effects
    private var applyTask: Task<Void, Never>?
    private var grantTask: Task<Void, Never>?

    init(effects: Effects) { self.effects = effects }

    /// The models the chosen engines want, plus the ones every install
    /// wants, whether on disk or not.
    static func wanted(_ inspection: Aria_Setup_V1_Inspection, _ selections: Aria_Setup_V1_Selections)
        -> [Aria_Setup_V1_Model]
    {
        inspection.models.filter { model in
            model.engineID.isEmpty || model.engineID == selections.transcriptionEngineID
                || model.engineID == selections.synthesisEngineID
        }
    }

    /// The wanted models that setup still has to fetch.
    static func plan(_ inspection: Aria_Setup_V1_Inspection, _ selections: Aria_Setup_V1_Selections)
        -> [Aria_Setup_V1_Model]
    {
        wanted(inspection, selections).filter { $0.onDisk != .complete && !$0.external }
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
    static func downloadSize(of models: [Aria_Setup_V1_Model]) -> String {
        let size = size(models.reduce(0) { $0 + $1.sizeBytes })
        return models.contains { $0.onDisk == .partial } ? "up to \(size)" : size
    }

    /// What picking `engine` downloads, for comparing engines before one
    /// is chosen.
    static func download(of engine: Aria_Setup_V1_Choice, in inspection: Aria_Setup_V1_Inspection) -> EngineDownload {
        switch engine.modelSource {
        case .downloads: break
        case .fetchedOnFirstUse: return .status("Fetched on first use")
        case .builtIn: return .status("Built in")
        case .remote: return .status("On the server")
        case .unspecified, .UNRECOGNIZED: return .status("Size unknown")
        }
        let models = inspection.models.filter { $0.engineID == engine.id }
        if models.contains(where: \.external) { return .status("Custom model") }
        let missing = models.filter { $0.onDisk != .complete }
        if missing.isEmpty { return .status("Downloaded") }
        return .size(downloadSize(of: missing))
    }

    /// Why the choices cannot be applied: an engine this Mac cannot run.
    private static func blocked(_ selections: Aria_Setup_V1_Selections, in inspection: Aria_Setup_V1_Inspection)
        -> String?
    {
        let chosen = [
            (selections.transcriptionEngineID, inspection.transcriptionChoices),
            (selections.synthesisEngineID, inspection.synthesisChoices),
        ]
        for (key, engines) in chosen {
            guard let engine = engines.first(where: { $0.id == key }) else { continue }
            if engine.availability == .unsupported { return "\(engine.label): \(engine.availabilityDetail)" }
        }
        return nil
    }

    /// The wizard opens itself once, and only while something is missing.
    static func shouldOffer(missing: [Aria_Setup_V1_Missing], offeredBefore: Bool) -> Bool {
        !missing.isEmpty && !offeredBefore
    }

    /// A voice can be recorded once its model is on disk and nobody is
    /// enrolled yet.
    static func offersVoice(_ summary: Aria_Setup_V1_Summary, speakers: [Aria_Daemon_V1_EnrolledSpeaker]) -> Bool {
        speakers.isEmpty && summary.voiceModelPresent
    }

    var blockedBy: String? { Self.blocked(selections, in: inspection) }

    /// What the wizard still has to do on this Mac, as one line; nothing
    /// until the Mac has been looked at.
    var missingSummary: String? {
        guard step != .inspecting else { return nil }
        let names = inspection.missing.map { missing in
            switch missing.kind {
            case .settings: "settings"
            case .wakeword: "wake-word models"
            case .model: "models"
            case .service: "background service"
            case .grant: "Aria key permission"
            case .unspecified, .UNRECOGNIZED: "unknown"
            }
        }
        var seen = Set<String>()
        let unique = names.filter { seen.insert($0).inserted }
        return unique.isEmpty ? "Everything is in place." : "Missing: \(unique.joined(separator: ", "))."
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
            (selections.transcriptionEngineID, inspection.transcriptionChoices),
            (selections.synthesisEngineID, inspection.synthesisChoices),
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

    func title(ofModel id: String) -> String {
        inspection.models.first { $0.id == id }?.title ?? id
    }

    static func size(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }

    func start() {
        switch step {
        case .inspecting, .welcome, .done, .failed:
            step = .inspecting
            Task { await inspectNow() }
        case .grant:
            watchGrant()
        default:
            return
        }
    }

    func stop() {
        grantTask?.cancel()
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
            finish()
        default:
            break
        }
    }

    private func finish() {
        let problems = [effects.launchAtLogin(), effects.installCommandLineTool()].compactMap { $0 }
        notice = problems.isEmpty ? nil : problems.joined(separator: " ")
        step = .done
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

    func openAccessibility() {
        effects.openAccessibility()
    }

    func recordVoice() {
        let name = voiceName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, !isBusy else { return }
        isBusy = true
        notice = nil
        voicePrompt = "Starting…"
        Task {
            do {
                _ = try await effects.enroll(name)
                enrolled = "Voice saved for \(name)."
                step = .tryOut
            } catch {
                notice = Self.notice(for: error, during: "enroll")
            }
            isBusy = false
            voicePrompt = nil
        }
    }

    /// What the daemon says between samples; shown only while recording.
    func speakerEvent(_ event: Aria_Daemon_V1_SpeakerEvent) {
        guard step == .voice, isBusy else { return }
        switch event.stage {
        case .recording: voicePrompt = "Say something (\(event.index)/\(event.samples))…"
        case .sampleTaken: voicePrompt = "Heard (\(event.index)/\(event.samples))"
        default: break
        }
    }

    func tryPress() {
        guard !isBusy else { return }
        isBusy = true
        notice = nil
        heard = nil
        Task {
            defer { isBusy = false }
            do {
                heard = try await effects.transcribe()
            } catch {
                notice = Self.notice(for: error, during: "try-out transcription", otherwise: Self.transcribeFailure)
                return
            }
            do {
                try await effects.speak(Self.spokenReply)
                finish()
            } catch {
                notice = Self.notice(for: error, during: "try-out reply", otherwise: Self.speakFailure)
            }
        }
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
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = selections
        request.downloads = Self.plan(inspection, selections).map(\.id).filter(downloads.contains)
        applyTask = Task {
            do {
                let summary = try await SetupClient.handingOffDaemon(
                    inspection, stop: effects.stopDaemon, start: effects.startDaemon
                ) {
                    try await effects.apply(request) { event in
                        Task { @MainActor [weak self] in self?.absorb(event) }
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
        case .download(let download): downloadProgress[download.modelID] = download
        case nil: break
        }
    }

    private func finishApply(_ summary: Aria_Setup_V1_Summary) async {
        self.summary = summary
        isHotkeyReady = selections.ariaKey && summary.grant == .armed
        if selections.ariaKey && summary.grant != .armed {
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
                let armed = await effects.hotkeyArmed()
                // Skip may have moved on while the check was out.
                guard !Task.isCancelled else { return }
                if armed {
                    isHotkeyReady = true
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
    static let transcribeFailure = "Aria couldn’t transcribe that. Try again."
    static let speakFailure = "Aria couldn’t read the reply aloud. Try again."
    static let unknownFailure = "Something went wrong. Try again."

    /// Speaker and try-out errors carry text written for the user. Any other
    /// error's text goes to the log instead.
    private static func message(
        for error: any Error, during step: String, otherwise fallback: String = unknownFailure
    ) -> String {
        if error is SpeakerFailure || error is TryOutError { return error.localizedDescription }
        log.error("Setup \(step, privacy: .public) failed: \(String(describing: error), privacy: .private)")
        if case .ariaBinaryMissing? = error as? DaemonClientError { return missingBinary }
        return fallback
    }

    /// Nil for a cancellation, which needs no notice.
    private static func notice(
        for error: any Error, during step: String, otherwise fallback: String = unknownFailure
    ) -> String? {
        DaemonTransport.isCancellation(error) ? nil : message(for: error, during: step, otherwise: fallback)
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
        case .done, .failed: nil
        }
    }

    /// Where the step sits in the walk, so pages slide the right way.
    /// Inspecting and welcome share a page, so they share a slot.
    var order: Int {
        if let stage { return stage.rawValue }
        let stages = SetupProgress.Stage.allCases.count
        return self == .done ? stages : stages + 1
    }

    var title: String {
        switch self {
        case .inspecting, .welcome: "Welcome to Aria"
        case .choices: "Choose Your Engines"
        case .applying: "Setting Up…"
        case .grant: "Allow Access"
        case .voice: "Your Voice"
        case .tryOut: "Try It"
        case .done: "Aria Is Ready"
        case .failed: "Setup Didn’t Finish"
        }
    }

    var caption: String? {
        switch self {
        case .inspecting, .welcome: "Dictation stays on your Mac. Ask uses your selected assistant."
        case .choices: "Speech stays on your Mac. You can change these later."
        case .applying: "This takes a moment. Downloads can be canceled."
        case .grant: "Aria uses it to hear the Aria key and type what you say."
        case .voice: "Aria can tell who is speaking once it knows your voice."
        case .tryOut: "Click **Record** and say a few words. Nothing is typed into other apps."
        case .done: "Aria runs in the background. Find it in the menu bar."
        case .failed: nil
        }
    }
}
