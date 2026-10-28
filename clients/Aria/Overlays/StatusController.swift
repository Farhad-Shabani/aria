import Combine
import Foundation

/// Shows the status pill while a press or an ask is in flight, and tells
/// the menu bar what it would show.
@MainActor
final class StatusController: ObservableObject {
    static let enabledKey = "statusPillEnabled"
    static let isEnabledByDefault = true

    /// What the pill would show, for the menu bar, even with the pill turned off.
    @Published private(set) var live: PillState?

    private let events: DaemonEvents
    private let prompts: PromptHotkeys
    private let colors: VoiceColors
    private var heard = HeardSpeaker()
    private let panel = FloatingPanel(width: OverlayStyle.pillWidth, bottomMargin: OverlayStyle.pillBottomMargin)
    private var foregroundState: PillState?
    private var foregroundRun: RunID?
    /// The recording the daemon opened last. Only its opening event names it.
    private var lastRecording: UInt64?
    private var asks = AsksInFlight()
    private var runsChange: AnyCancellable?
    private var shown: Pill?
    /// The microphone, as of the last level the daemon sent. The ribbon
    /// reads it every frame, so a new level needs no redraw of the pill.
    private let level = LiveLevel()
    /// Clears a finished press once its words have been read.
    private var expiry: Task<Void, Never>?

    /// Shares the caption's stream: one subscription feeds every overlay.
    init(events: DaemonEvents, prompts: PromptHotkeys, colors: VoiceColors) {
        self.events = events
        self.prompts = prompts
        self.colors = colors
    }

    /// The caller starts the stream once every overlay has registered.
    func start() {
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.isEnabledByDefault])
        events.onDictation { [weak self] event in
            guard let self else { return }
            guard let event else {
                self.heard.apply(nil)
                self.lastRecording = nil
                self.show(nil)
                return
            }
            if event.stage == .recording { self.lastRecording = event.recordingID }
            self.hear(event)
            guard let state = PillState(event) else { return }
            self.show(state, run: self.lastRecording.map(RunID.recording))
        }
        events.onChat { [weak self] chat in
            guard let self else { return }
            if let chat, let ended = PillState(ended: chat) {
                self.show(ended, run: .ticket(chat.ticket))
            }
            self.asks.apply(chat)
            self.render()
        }
        events.onLevel { [weak self] level in
            self?.level.value = level
        }
        // A chord learns its recording and ticket on its own call, which can
        // land after the event that opened them, so a late name redraws.
        // Published values arrive before they are stored, hence the hop.
        runsChange = prompts.$runs.receive(on: DispatchQueue.main).sink { [weak self] _ in
            MainActor.assumeIsolated { self?.render() }
        }
        NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.render() }
        }
    }

    private func hear(_ event: Aria_Daemon_V1_DictationEvent) {
        heard.apply(event)
        render()
    }

    /// A press that ended keeps the pill for its own lifetime, then lets go.
    private func show(_ state: PillState?, run: RunID? = nil) {
        expiry?.cancel()
        expiry = nil
        foregroundState = state
        foregroundRun = run
        render()
        guard let life = state?.life else { return }
        expiry = Task { [weak self] in
            try? await Task.sleep(for: life)
            guard !Task.isCancelled else { return }
            self?.foregroundState = nil
            self?.render()
        }
    }

    private func render() {
        // A press is the person's turn; it outranks whatever an agent is doing.
        let live = foregroundState ?? PillState(asks: asks)
        if self.live != live { self.live = live }
        let run = foregroundState == nil ? asks.oldest.map { RunID.ticket($0.ticket) } : foregroundRun
        let enabled = UserDefaults.standard.bool(forKey: Self.enabledKey)
        // Only a press is spoken; an agent's step has no voice to show.
        let ring: PillRing? =
            switch foregroundRun {
            case .ticket: nil
            case .recording, nil: foregroundState == nil ? nil : heard.voice.map(ring(for:))
            }
        let wanted =
            enabled
            ? live.map { Pill(state: $0, prompt: run.flatMap(prompts.runs.prompt(of:)), ring: ring) } : nil
        guard wanted != shown else { return }
        shown = wanted
        // A new press starts from silence rather than the last one's level.
        if case .recording = wanted?.state { level.value = 0 }
        draw(wanted)
    }

    private func ring(for voice: HeardSpeaker.Voice) -> PillRing {
        switch voice {
        case .named(let name): .voice(colors.assignColor(to: name))
        case .keptOut: .keptOut
        }
    }

    private func draw(_ pill: Pill?) {
        if let pill {
            panel.show(StatusPillView(pill: pill, level: level))
        } else {
            panel.hide()
        }
    }
}
