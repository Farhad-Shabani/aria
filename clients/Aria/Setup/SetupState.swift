import Foundation

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
        case models
        case applying
        case grant
        case voice
        case tryOut
        case done
        case failed(String)
    }

    /// The footer dots. Inspecting, welcome, and failure sit outside them.
    enum Stage: Int, CaseIterable {
        case choose
        case download
        case install
        case finish
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
        (.config, "Configuration"), (.wakeword, "Wake-word models"), (.models, "Models"),
        (.service, "Background service"),
    ]

    @Published private(set) var step: Step = .inspecting {
        didSet { forward = step.order >= oldValue.order }
    }
    /// Whether the last step change moved ahead, so pages slide the right way.
    @Published private(set) var forward = true
    @Published private(set) var inspection = Aria_Setup_V1_Inspection()
    @Published var selections = Aria_Setup_V1_Selections()
    @Published var downloads = Set<String>()
    @Published private(set) var applying: Aria_Setup_V1_Step = .unspecified
    @Published private(set) var progress: [String: Aria_Setup_V1_DownloadProgress] = [:]
    @Published private(set) var summary: Aria_Setup_V1_Summary?
    @Published var voiceName = ""
    @Published private(set) var voicePrompt: String?
    @Published private(set) var enrolled: String?
    @Published private(set) var heard: String?
    /// A step's own failure, which setup carries on past.
    @Published private(set) var notice: String?
    @Published private(set) var busy = false

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
            model.engine.isEmpty || model.engine == selections.stt || model.engine == selections.tts
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
        plan(inspection, selections).filter(\.default)
    }

    static func total(_ models: [Aria_Setup_V1_Model]) -> String {
        models.isEmpty ? "No downloads needed." : "Total download: \(downloadSize(of: models))"
    }

    /// Says "up to" when a download resumes: only the transfer knows how
    /// much is left.
    static func downloadSize(of models: [Aria_Setup_V1_Model]) -> String {
        let size = size(models.reduce(0) { $0 + $1.bytes })
        return models.contains { $0.onDisk == .partial } ? "up to \(size)" : size
    }

    /// What picking `engine` downloads, for comparing engines before one
    /// is chosen.
    static func download(of engine: Aria_Setup_V1_Engine, in inspection: Aria_Setup_V1_Inspection) -> EngineDownload {
        switch engine.model {
        case .downloads: break
        case .fetchedOnFirstUse: return .status("Fetched on first use")
        case .builtIn: return .status("Built in")
        case .unspecified, .UNRECOGNIZED: return .status("Size unknown")
        }
        let models = inspection.models.filter { $0.engine == engine.key }
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
            (selections.stt, inspection.stt), (selections.tts, inspection.tts), (selections.chat, inspection.chat),
        ]
        for (key, engines) in chosen {
            guard let engine = engines.first(where: { $0.key == key }) else { continue }
            if engine.availability == .unsupported { return "\(engine.label): \(engine.note)" }
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
            case .config: "configuration"
            case .wakeword: "wake-word models"
            case .model: "models"
            case .service: "background service"
            case .grant: "Accessibility grant"
            case .unspecified, .UNRECOGNIZED: "unknown"
            }
        }
        var seen = Set<String>()
        let unique = names.filter { seen.insert($0).inserted }
        return unique.isEmpty ? "Everything is in place." : "Missing: \(unique.joined(separator: ", "))."
    }

    var downloadTotal: String {
        let plan = Self.plan(inspection, selections)
        let chosen = plan.filter { downloads.contains($0.name) }
        return chosen.isEmpty && !plan.isEmpty ? "Nothing selected." : Self.total(chosen)
    }

    /// One note per chosen engine that downloads its own model, which
    /// setup cannot size.
    var selfFetchingNotes: [String] {
        let chosen = [(selections.stt, inspection.stt), (selections.tts, inspection.tts)]
        return chosen.compactMap { key, engines in
            engines.first { $0.key == key && $0.model == .fetchedOnFirstUse }
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
            step = .choices
        case .choices:
            guard blockedBy == nil else { return }
            downloads = defaultDownloads
            step = .models
        case .models:
            runApply()
        case .grant:
            grantTask?.cancel()
            Task { await enterVoice() }
        case .voice:
            step = .tryOut
        case .tryOut:
            let problems = [effects.launchAtLogin(), effects.installCommandLineTool()].compactMap { $0 }
            notice = problems.isEmpty ? nil : problems.joined(separator: " ")
            step = .done
        default:
            break
        }
    }

    func back() {
        switch step {
        case .choices: step = .welcome
        case .models: step = .choices
        default: break
        }
    }

    private var defaultDownloads: Set<String> {
        Set(Self.expressPlan(inspection, selections).map(\.name))
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
        guard !name.isEmpty, !busy else { return }
        busy = true
        notice = nil
        voicePrompt = "Starting…"
        Task {
            do {
                let response = try await effects.enroll(name)
                let consistency = response.consistency.formatted(.number.precision(.fractionLength(2)))
                enrolled = "\(name) enrolled (consistency \(consistency))"
                step = .tryOut
            } catch {
                notice = error.localizedDescription
            }
            busy = false
            voicePrompt = nil
        }
    }

    /// What the daemon says between samples; shown only while recording.
    func speakerEvent(_ event: Aria_Daemon_V1_SpeakerEvent) {
        guard step == .voice, busy else { return }
        switch event.stage {
        case .recording: voicePrompt = "Say something (\(event.index)/\(event.samples))…"
        case .sampleTaken: voicePrompt = "Heard (\(event.index)/\(event.samples))"
        default: break
        }
    }

    func tryPress() {
        guard !busy else { return }
        busy = true
        notice = nil
        heard = nil
        Task {
            do {
                heard = try await effects.transcribe()
                try await effects.speak(Self.spokenReply)
            } catch {
                notice = error.localizedDescription
            }
            busy = false
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
            step = .failed(error.localizedDescription)
        }
    }

    private func runApply() {
        let origin = step
        step = .applying
        applying = .unspecified
        progress = [:]
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = selections
        request.downloads = Self.plan(inspection, selections).map(\.name).filter(downloads.contains)
        applyTask = Task {
            // The service refuses a socket some other daemon owns, and the
            // app spawns one itself while no service exists.
            let shouldStopDaemon = inspection.service.job == .absent
            if shouldStopDaemon { await effects.stopDaemon() }
            do {
                let summary = try await effects.apply(request) { event in
                    Task { @MainActor [weak self] in self?.absorb(event) }
                }
                await finishApply(summary)
            } catch SetupClientError.cancelled {
                if shouldStopDaemon { await effects.startDaemon() }
                step = origin
            } catch {
                if shouldStopDaemon { await effects.startDaemon() }
                step = .failed(error.localizedDescription)
            }
        }
    }

    private func absorb(_ event: Aria_Setup_V1_SetupEvent) {
        switch event.event {
        case .started(let step): applying = step
        case .download(let download): progress[download.model] = download
        case nil: break
        }
    }

    private func finishApply(_ summary: Aria_Setup_V1_Summary) async {
        self.summary = summary
        if selections.hotkey && summary.grant != .armed {
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
            notice = error.localizedDescription
            step = .tryOut
        }
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
    var stage: SetupState.Stage? {
        switch self {
        case .inspecting, .welcome, .failed: nil
        case .choices: .choose
        case .models: .download
        case .applying: .install
        case .grant, .voice, .tryOut, .done: .finish
        }
    }

    /// Where the step sits in the walk. Inspecting and welcome share a
    /// page, so they share a slot.
    var order: Int {
        switch self {
        case .inspecting, .welcome: 0
        case .choices: 1
        case .models: 2
        case .applying: 3
        case .grant: 4
        case .voice: 5
        case .tryOut: 6
        case .done: 7
        case .failed: 8
        }
    }

    var title: String {
        switch self {
        case .inspecting, .welcome: "Welcome to Aria"
        case .choices: "Choose your engines"
        case .models: "Download models"
        case .applying: "Setting up…"
        case .grant: "Allow the hotkey"
        case .voice: "Your voice"
        case .tryOut: "Try it"
        case .done: "Aria is ready"
        case .failed: "Setup did not finish"
        }
    }

    var caption: String? {
        switch self {
        case .inspecting, .welcome: "Dictation stays on this Mac. Ask goes through Claude Code or Codex."
        case .choices: "Speech stays on this Mac. You can change these later."
        case .models: "Downloads land in ~/.aria and resume if interrupted."
        case .applying: "This takes a moment. Downloads can be cancelled."
        case .grant: "The dictation hotkey needs Aria's Accessibility grant."
        case .voice: "Aria can tell who is speaking once it knows your voice."
        case .tryOut: "One press, start to finish. Nothing is typed into any window."
        case .done: "Aria runs in the background. Find it in the menu bar."
        case .failed: nil
        }
    }
}
