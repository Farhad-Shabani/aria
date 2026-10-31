import Combine

/// The prompts that are on and the chords they hold: what `PromptHotkeys`
/// acts on, decided apart from Carbon.
struct PromptChords: Equatable {
    let enabled: [String]
    let chords: [String: Chord]

    init(_ entries: [Aria_Daemon_V1_Prompt]) {
        let on = entries.filter(\.enabled)
        enabled = on.map(\.id)
        chords = Dictionary(
            on.compactMap { entry in entry.storedChord.map { (entry.id, $0) } },
            uniquingKeysWith: { first, _ in first }
        )
    }
}

@MainActor
final class PromptHotkeys: ObservableObject {
    /// Prompts whose chord Carbon turned down.
    @Published private(set) var refused: Set<String> = []
    @Published private(set) var runs = PromptRuns()
    /// The name of each prompt a press turned down because its file can't
    /// be read.
    let refusals = PassthroughSubject<String, Never>()

    private let events: DaemonEvents
    // A controller dropped with its prompt leaves its chat subscription
    // behind. A session removes few enough prompts for that to stay small.
    private var controllers: [String: PressController] = [:]
    private var entries: [Aria_Daemon_V1_Prompt] = []
    private var promptsChange: AnyCancellable?

    init(events: DaemonEvents) {
        self.events = events
        // A lost stream may be a restarted daemon, which numbers its tickets
        // afresh.
        events.onChat { [weak self] chat in
            if chat == nil { self?.runs = PromptRuns() }
        }
    }

    func track(_ settings: DaemonSettings) {
        promptsChange = settings.$snapshot
            .map { $0?.prompts.entries ?? [] }
            .removeDuplicates()
            .sink { [weak self] entries in
                Task { @MainActor in self?.apply(entries) }
            }
    }

    /// The prompts that are on, in Settings order.
    var runnable: [Aria_Daemon_V1_Prompt] {
        entries.filter(\.enabled)
    }

    func press(_ id: String) {
        controllers[id]?.press()
    }

    func isBusy(_ id: String) -> Bool {
        controllers[id]?.isBusy ?? false
    }

    private func apply(_ entries: [Aria_Daemon_V1_Prompt]) {
        self.entries = entries
        let plan = PromptChords(entries)
        // Every chord that moves is let go first, so a prompt taking the
        // chord another one just gave up finds it free.
        for (id, controller) in controllers where controller.chord.bound != plan.chords[id] {
            controller.chord.bind(nil)
        }
        // A prompt turned off keeps its controller, so turning it back on
        // starts no second chat subscription.
        let ids = Set(entries.map(\.id))
        controllers = controllers.filter { ids.contains($0.key) }
        for id in ids {
            let controller =
                controllers[id]
                ?? PressController(
                    events: events,
                    operation: .prompt(id: id, canRun: { [weak self] in self?.canRun(id) ?? false })
                ) { [weak self] started in
                    self?.note(started, prompt: id)
                }
            controllers[id] = controller
            controller.chord.bind(plan.chords[id])
        }
        refused = Set(controllers.filter { $0.value.chord.wasRefused }.keys)
    }

    private func canRun(_ id: String) -> Bool {
        guard let entry = entries.first(where: { $0.id == id }) else { return false }
        guard PromptFile.isReadable(entry) else {
            refusals.send(entry.name)
            return false
        }
        return true
    }

    /// Named as the prompt is called now, so the pill matches Settings.
    private func note(_ run: RunID, prompt id: String) {
        guard let name = entries.first(where: { $0.id == id })?.name else { return }
        runs.add(run, prompt: name)
    }
}
