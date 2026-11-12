import AppKit
import Combine

/// Shows the status pill while a press or a chat is in flight, and tells
/// the menu bar what it would show.
@MainActor
final class StatusController: ObservableObject {
    static let enabledKey = "statusPillEnabled"
    static let isEnabledByDefault = true

    /// What the pill would show, for the menu bar, even with the pill turned off.
    @Published private(set) var live: PillState?

    private let events: DaemonEvents
    private let prompts: PromptHotkeys
    /// Tickets something else shows.
    var skips: @MainActor (UInt64) -> Bool = { _ in false }
    private var heard = HeardSpeaker()
    private let panel = FloatingPanel(width: OverlayStyle.pillWidth, bottomMargin: OverlayStyle.pillBottomMargin)
    private var foregroundState: PillState?
    private var foregroundRun: RunID?
    /// The recording the daemon opened last. Only its opening event names it.
    private var lastRecording: UInt64?
    /// The app in front when that recording opened: where its words go.
    private var startApp = StartApp(bundleID: nil)
    /// That recording's route, which also only its opening event names.
    private var route = Aria_Daemon_V1_TranscriptRoute.unspecified
    private var asks = AsksInFlight()
    /// The last recording a chat press started. Its events carry no sign of
    /// the chat, so the press reports it.
    private var askRecording: RunID?
    private var runsChange: AnyCancellable?
    private var refusalsChange: AnyCancellable?
    private var shown: Pill?
    /// The microphone, as of the last level the daemon sent. The ribbon
    /// reads it every frame, so a new level needs no redraw of the pill.
    private let level = LiveLevel()
    /// Clears a finished press once its words have been read.
    private var expiry: Task<Void, Never>?

    /// Shares the caption's stream: one subscription feeds every overlay.
    init(events: DaemonEvents, prompts: PromptHotkeys) {
        self.events = events
        self.prompts = prompts
    }

    /// The caller starts the stream once every overlay has registered.
    func start() {
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.isEnabledByDefault])
        events.onDictation { [weak self] event in
            guard let self else { return }
            guard let event else {
                self.heard.apply(nil)
                self.lastRecording = nil
                // A new daemon numbers its recordings from 1 again.
                self.askRecording = nil
                self.show(nil)
                return
            }
            if event.stage == .recording {
                self.lastRecording = event.recordingID
                self.route = event.route
                self.startApp = StartApp(bundleID: NSWorkspace.shared.frontmostApplication?.bundleIdentifier)
            }
            self.hear(event)
            if let state = self.foregroundState?.segment(event.stage, startApp: self.startApp) {
                self.show(state, run: self.foregroundRun)
                return
            }
            // It follows the terminal stage, whose pill it keeps.
            if event.stage == .returnDropped {
                self.show(.returnDropped(after: self.foregroundState), run: self.lastRecording.map(RunID.recording))
                return
            }
            guard let state = PillState(event) else { return }
            let run = self.lastRecording.map(RunID.recording)
            guard let shown = state.pill(for: self.route) else {
                // Text handed back ends only its own recording's pill, never a
                // refusal that beat its last event here.
                if self.foregroundRun == run { self.show(nil) }
                return
            }
            self.show(shown, run: run)
        }
        events.onChat { [weak self] chat in
            guard let self else { return }
            // Events can come before the owner learns its ticket, so the chat may already be in
            // flight here. It ends now, since its reply is skipped.
            if let chat, skips(chat.ticket) {
                self.asks.forget(chat.ticket)
                self.render()
                return
            }
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
        refusalsChange = prompts.refusals.sink { [weak self] refusal in
            MainActor.assumeIsolated { self?.show(.refused(prompt: refusal.prompt, failure: refusal.failure)) }
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
        let voice: VoiceEmphasis? =
            switch foregroundRun {
            case .ticket: nil
            case .recording, nil: foregroundState == nil ? nil : heard.voice.map(VoiceEmphasis.init)
            }
        let prompt = run.flatMap(prompts.runs.prompt(of:))
        let glows = Pill.glows(run: run, chat: askRecording, prompt: prompt)
        let wanted = enabled ? live.map { Pill(state: $0, prompt: prompt, voice: voice, glows: glows) } : nil
        guard wanted != shown else { return }
        shown = wanted
        // A new press starts from silence rather than the last one's level.
        if case .recording = wanted?.state { level.value = 0 }
        draw(wanted)
    }

    func noteDelivery(_ outcome: Result<Aria_Daemon_V1_DeliveryOutcome, any Error>) {
        switch outcome {
        case .success(.copied): show(.replied(copied: true))
        case .success: break
        case .failure: show(.failed("couldn’t type"))
        }
    }

    func noteAsk(_ run: RunID) {
        // A ticket is Aria's work anyway; only the recording needs marking.
        guard case .recording = run else { return }
        askRecording = run
        render()
    }

    private func draw(_ pill: Pill?) {
        if let pill {
            panel.show(StatusPillView(pill: pill, level: level))
        } else {
            panel.hide()
        }
    }
}
