import AppKit
import SwiftUI

/// What the pill shows. Every press passes through these in order, and the
/// ones that end a press say so before they go.
enum PillState: Equatable {
    case recording(since: Date)
    case transcribing
    case delivering
    /// The press delivered: where, and how many words.
    case delivered(DeliveryOutcome, words: Int)
    case cancelled
    /// Focus moved, so typing stopped. Not a failure.
    case interrupted
    /// A press that failed, in the words the person needs.
    case failed(String)
    /// An ask in flight: `claude · reading`, and since when.
    case asking(String, since: Date)
    /// An ask that came back, by agent. Shown the way a delivered press is.
    case replied(String)
    /// An ask that did not, by agent. Shown the way a failed press is.
    case noReply(String)

    init?(_ event: Aria_Daemon_V1_DictationEvent?, now: Date = Date()) {
        guard let event else { return nil }
        switch event.stage {
        case .recording: self = .recording(since: now)
        case .transcribing: self = .transcribing
        case .delivering: self = .delivering
        case .delivered: self = .delivered(DeliveryOutcome(event.outcome), words: Int(event.wordCount))
        case .cancelled: self = .cancelled
        case .failed where [.deliveryTargetChanged, .deliveryInterrupted].contains(event.errorCode):
            self = .interrupted
        case .failed: self = .failed(event.errorCode.dictationMessage)
        // A segment lands while the speaker is still talking, so it says
        // nothing about the press.
        case .segmentTranscribed, .segmentDelivered: return nil
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    /// The pill for an ask that just ended, or nothing for one under way.
    init?(ended event: Aria_Daemon_V1_ChatEvent) {
        switch event.stage {
        case .replied: self = .replied(event.agent.shortName)
        case .failed: self = .noReply(event.agent.shortName)
        default: return nil
        }
    }

    /// The pill for the asks in flight: the oldest one's agent and step,
    /// and how many more wait behind it.
    init?(asks: AsksInFlight) {
        guard let oldest = asks.oldest else { return nil }
        var label = "\(oldest.agent.shortName) · \(oldest.stepLabel)"
        if asks.count > 1 {
            label += " · +\(asks.count - 1)"
        }
        self = .asking(label, since: oldest.since)
    }

    /// How long the pill stays once the press is over. A live press has no
    /// life of its own: the next event ends it.
    var life: Duration? {
        switch self {
        case .recording, .transcribing, .delivering, .asking: return nil
        case .delivered, .replied: return .milliseconds(1200)
        case .cancelled: return .milliseconds(800)
        case .interrupted: return .seconds(2)
        // Long enough to read, and the next press clears it anyway.
        case .failed, .noReply: return .seconds(5)
        }
    }

    var tint: Color {
        switch self {
        case .recording, .cancelled, .failed, .noReply:
            return Color(red: 0.88, green: 0.42, blue: 0.45)
        case .transcribing, .delivering, .asking:
            return Color(red: 0.90, green: 0.75, blue: 0.48)
        case .delivered, .replied:
            return Color(red: 0.48, green: 0.80, blue: 0.59)
        case .interrupted:
            return Color(white: 0.72)
        }
    }

    /// An agent at work breathes; everything else holds still. A live
    /// microphone draws the ribbon instead.
    var motion: Motion {
        switch self {
        case .asking, .delivering: return .breathe
        case .recording, .transcribing, .delivered, .cancelled, .interrupted, .failed, .replied, .noReply:
            return .still
        }
    }

    /// The words beside the mark, or none where the mark says it all.
    var label: String? {
        switch self {
        case .recording: return nil
        case .transcribing: return "transcribing"
        case .delivering: return "typing"
        case .delivered(let outcome, let words):
            let count = words == 1 ? "1 word" : "\(words) words"
            return outcome == .copied ? "copied · \(count)" : count
        case .cancelled: return "cancelled"
        case .interrupted: return "stopped · focus moved"
        case .failed(let message): return message
        case .asking(let label, _): return label
        case .replied(let agent): return "\(agent) · replied"
        case .noReply(let agent): return "\(agent) · no reply"
        }
    }

    /// The clock the pill runs, for the states worth timing.
    var since: Date? {
        switch self {
        case .recording(let since), .asking(_, let since): return since
        default: return nil
        }
    }
}

/// Where a delivered transcript went. Mirrors the daemon's own
/// `DeliveryOutcome`.
enum DeliveryOutcome: Equatable {
    case inserted
    case copied
    /// A press that asked only for its transcript, or a daemon too old to say.
    case unknown

    init(_ code: Aria_Daemon_V1_DeliveryOutcome) {
        switch code {
        case .inserted: self = .inserted
        case .copied: self = .copied
        case .unspecified, .UNRECOGNIZED: self = .unknown
        }
    }

}

extension Aria_Daemon_V1_ErrorCode {
    /// What a failed press says. The daemon logs a code; a person reads this.
    var dictationMessage: String {
        switch self {
        case .noSpeech: return "No speech detected"
        case .captureFailed: return "Recording failed — check your mic"
        case .transcriptionFailed: return "Transcription failed"
        case .deliveryFailed, .deliveryTimeout: return "The transcript could not be delivered"
        case .recordingTimeout, .overallTimeout: return "The press ran past its time limit"
        default: return "Dictation failed"
        }
    }
}

enum Motion: Equatable {
    case breathe
    case still

    var scale: CGFloat {
        switch self {
        case .breathe: return 1.25
        case .still: return 1
        }
    }

    var animation: Animation? {
        switch self {
        case .breathe: return .easeInOut(duration: 1.4).repeatForever()
        case .still: return nil
        }
    }
}

/// One ask the daemon has not answered yet.
struct Ask: Equatable {
    var agent: Aria_Daemon_V1_ChatAgent
    var step: Aria_Daemon_V1_ChatStep
    var skill: String
    var since: Date

    var stepLabel: String {
        step == .skill ? "skill: \(skill)" : step.label
    }
}

/// Which asks the daemon has not answered yet, by ticket.
struct AsksInFlight: Equatable {
    private var asks: [UInt64: Ask] = [:]

    var count: Int { asks.count }
    var oldest: Ask? { asks.min(by: { $0.key < $1.key })?.value }

    /// `nil` is the stream going away, and every ask with it.
    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?, now: Date = Date()) {
        guard let event else {
            asks = [:]
            return
        }
        switch event.stage {
        case .working:
            var ask =
                asks[event.ticket] ?? Ask(agent: event.agent, step: .thinking, skill: "", since: now)
            ask.step = event.step
            ask.skill = event.skill
            asks[event.ticket] = ask
        case .replied, .failed: asks[event.ticket] = nil
        default: break
        }
    }
}

extension Aria_Daemon_V1_ChatStep {
    /// The verb the pill shows; `skill` is completed by the skill's name.
    var label: String {
        switch self {
        case .thinking: return "thinking"
        case .reading: return "reading"
        case .searching: return "searching"
        case .running: return "running"
        case .skill: return "skill"
        case .unspecified, .UNRECOGNIZED: return "unknown"
        }
    }
}

extension Aria_Daemon_V1_ChatAgent {
    /// The short name the pill has room for.
    var shortName: String {
        switch self {
        case .claudeCode: return "claude"
        case .codex: return "codex"
        case .unspecified, .UNRECOGNIZED: return "agent"
        }
    }
}

/// Shows the status pill while a press or an ask is in flight, and tells
/// the menu bar what it would show.
@MainActor
final class StatusController: ObservableObject {
    static let enabledKey = "statusPillEnabled"
    static let enabledByDefault = true

    /// What the pill would show, for the menu bar, even with the pill turned off.
    @Published private(set) var live: PillState?

    private let events: DaemonEvents
    private let panel = FloatingPanel(width: pillWidth, bottomMargin: pillBottomMargin)
    private var dictation: PillState?
    private var asks = AsksInFlight()
    private var shown: PillState?
    /// The microphone, as of the last level the daemon sent. The ribbon
    /// reads it every frame, so a new level needs no redraw of the pill.
    private let level = LiveLevel()
    /// Clears a finished press once its words have been read.
    private var expiry: Task<Void, Never>?

    /// Shares the caption's stream: one subscription feeds every overlay.
    init(events: DaemonEvents) {
        self.events = events
    }

    /// The caller starts the stream once every overlay has registered.
    func start() {
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.enabledByDefault])
        events.onDictation { [weak self] event in
            guard let self else { return }
            guard let event else {
                self.show(nil)
                return
            }
            guard let state = PillState(event) else { return }
            self.show(state)
        }
        events.onChat { [weak self] chat in
            guard let self else { return }
            if let chat, let ended = PillState(ended: chat) {
                self.show(ended)
            }
            self.asks.apply(chat)
            self.render()
        }
        events.onLevel { [weak self] level in
            self?.meter(level)
        }
        NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.render() }
        }
    }

    /// A press that ended keeps the pill for its own lifetime, then lets go.
    private func show(_ state: PillState?) {
        expiry?.cancel()
        expiry = nil
        dictation = state
        render()
        guard let life = state?.life else { return }
        expiry = Task { [weak self] in
            try? await Task.sleep(for: life)
            guard !Task.isCancelled else { return }
            self?.dictation = nil
            self?.render()
        }
    }

    private func meter(_ level: Float) {
        self.level.value = level
    }

    private func render() {
        // A press is the person's turn; it outranks whatever an agent is doing.
        let live = dictation ?? PillState(asks: asks)
        if self.live != live { self.live = live }
        let enabled = UserDefaults.standard.bool(forKey: Self.enabledKey)
        let wanted = enabled ? live : nil
        guard wanted != shown else { return }
        shown = wanted
        // A new press starts from silence rather than the last one's level.
        if case .recording = wanted { level.value = 0 }
        draw(wanted)
    }

    private func draw(_ state: PillState?) {
        if let state {
            panel.show(StatusPillView(state: state, level: level))
        } else {
            panel.hide()
        }
    }
}

/// Room for the longest failure message; the capsule centres in it.
private let pillWidth: CGFloat = 480
/// The ribbon and the bar share one slot, so a press never jumps as the
/// mic gives way to the machine. A glyph is its own size, as on the board.
private let markWidth = RibbonTuning.width
private let markHeight = RibbonTuning.height
private let pillBottomMargin: CGFloat = 34

/// A mark and, where there is something to read, a word for it.
struct StatusPillView: View {
    let state: PillState
    let level: LiveLevel

    var body: some View {
        HStack(spacing: 7) {
            Mark(state: state, level: level)
            if state.label != nil || state.since != nil {
                HStack(spacing: 4) {
                    if let label = state.label {
                        Text(label)
                    }
                    if let since = state.since {
                        if state.label != nil {
                            Text("·")
                        }
                        Text(since, style: .timer)
                    }
                }
                .font(.system(size: 12, weight: .medium))
                .monospacedDigit()
                .foregroundStyle(.white)
                .padding(.trailing, 4)
            }
        }
        .padding(9)
        .background(.black.opacity(0.75), in: Capsule())
    }
}

/// The shape that carries the state. One slot, so nothing jumps as a press
/// moves from speaking to typing: a ribbon that follows the voice and then
/// flattens into a line, a bar that fills while text goes in, a glyph once
/// it is over.
private struct Mark: View {
    let state: PillState
    let level: LiveLevel

    var body: some View {
        switch state {
        case .recording, .transcribing:
            // One branch, so the ribbon's motion carries across the hand-off.
            Ribbon(state: state, level: level)
        case .delivering:
            Bar(tint: state.tint)
        case .delivered(.copied, _):
            Glyph(name: "doc.on.clipboard", tint: state.tint)
        case .delivered, .replied:
            Glyph(name: "checkmark", tint: state.tint)
        case .cancelled:
            Glyph(name: "xmark", tint: state.tint)
        case .interrupted:
            Glyph(name: "hand.raised", tint: state.tint)
        case .failed, .noReply, .asking:
            // Keyed on the motion: a new motion is a new dot with its own
            // loop, while a label change under the same motion keeps this one.
            Dot(tint: state.tint, motion: state.motion)
                .id(state.motion)
        }
    }
}

/// One bar in the ribbon's slot, filling left to right while text goes in,
/// segment by segment.
private struct Bar: View {
    let tint: Color

    @State private var running = false

    var body: some View {
        Capsule()
            .fill(.white.opacity(0.18))
            .frame(width: markWidth, height: 3)
            .overlay(alignment: .leading) {
                Capsule()
                    .fill(tint)
                    .frame(width: markWidth, height: 3)
                    .scaleEffect(x: running ? 1 : 0.001, anchor: .leading)
            }
            .clipShape(Capsule())
            .animation(.easeOut(duration: 1.2).repeatForever(autoreverses: false), value: running)
            .onAppear { running = true }
    }
}

private struct Glyph: View {
    let name: String
    let tint: Color

    var body: some View {
        Image(systemName: name)
            .font(.system(size: 10, weight: .bold))
            .foregroundStyle(tint)
            .frame(width: markHeight, height: markHeight)
    }
}

private struct Dot: View {
    let tint: Color
    let motion: Motion

    @State private var swelling = false

    var body: some View {
        Circle()
            .fill(tint)
            .frame(width: 10, height: 10)
            .scaleEffect(swelling ? motion.scale : 1)
            .animation(motion.animation, value: swelling)
            .onAppear { swelling = motion != .still }
    }
}
