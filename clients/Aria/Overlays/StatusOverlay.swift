import AppKit
import Combine
import SwiftUI

enum PillState: Equatable {
    case recording(since: Date)
    case transcribing
    case delivering
    /// The press delivered: where, and how many words.
    case delivered(DeliveryOutcome, words: Int)
    case cancelled
    /// Focus moved, so typing stopped. Not a failure.
    case interrupted
    /// The app in front has Aria switched off, by name when known.
    case excluded(String?)
    /// A press that failed, in the words the person needs.
    case failed(String)
    /// An ask in flight: its step, such as `reading · +1`, and since when.
    case asking(String, since: Date)
    case replied(copied: Bool)
    case noReply

    init?(
        _ event: Aria_Daemon_V1_DictationEvent?,
        now: Date = Date(),
        frontApp: @autoclosure () -> String? = NSWorkspace.shared.frontmostApplication?.localizedName
    ) {
        guard let event else { return nil }
        switch event.stage {
        case .recording: self = .recording(since: now)
        case .transcribing: self = .transcribing
        case .delivering: self = .delivering
        case .delivered: self = .delivered(DeliveryOutcome(event.outcome), words: Int(event.wordCount))
        case .cancelled: self = .cancelled
        case .failed where [.deliveryTargetChanged, .deliveryInterrupted].contains(event.errorCode):
            self = .interrupted
        case .failed where event.errorCode == .applicationExcluded: self = .excluded(frontApp())
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
        case .replied: self = .replied(copied: event.copied)
        case .failed: self = .noReply
        default: return nil
        }
    }

    /// The pill for the asks in flight: the oldest one's step, and how many
    /// others are in flight.
    init?(asks: AsksInFlight) {
        guard let oldest = asks.oldest else { return nil }
        var step = oldest.stepLabel
        if asks.count > 1 {
            step += " · +\(asks.count - 1)"
        }
        self = .asking(step, since: oldest.since)
    }

    /// How long the pill stays once the press is over. A live press has no
    /// life of its own: the next event ends it.
    var life: Duration? {
        switch self {
        case .recording, .transcribing, .delivering, .asking: return nil
        case .delivered, .replied: return .milliseconds(1200)
        case .cancelled: return .milliseconds(800)
        case .interrupted, .excluded: return .seconds(2)
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
        case .interrupted, .excluded:
            return Color(white: 0.72)
        }
    }

    var motion: Motion {
        switch self {
        case .asking, .delivering: return .breathe
        case .recording, .transcribing, .delivered, .cancelled, .interrupted, .excluded, .failed, .replied,
            .noReply:
            return .still
        }
    }

    /// The words beside the mark, or none where the mark says it all. A
    /// prompt's name leads its own run, so a wrong chord shows before
    /// anything is said.
    func label(prompt: String? = nil) -> String? {
        let words = [prompt, detail].compactMap { $0 }
        return words.isEmpty ? nil : words.joined(separator: " · ")
    }

    private var detail: String? {
        switch self {
        case .recording: return nil
        case .transcribing: return "transcribing"
        case .delivering: return "typing"
        case .delivered(let outcome, let words):
            let count = words == 1 ? "1 word" : "\(words) words"
            return outcome == .copied ? "copied · \(count)" : count
        case .cancelled: return "cancelled"
        case .interrupted: return "stopped · focus moved"
        case .excluded(let app): return "off in \(app ?? "this app")"
        case .failed(let message): return message
        case .asking(let step, _): return step
        case .replied(let copied): return copied ? "copied" : "replied"
        case .noReply: return "no reply"
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
    let ticket: UInt64
    var step: Aria_Daemon_V1_ChatStep
    var skill: String
    let since: Date

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
                asks[event.ticket]
                ?? Ask(ticket: event.ticket, step: .thinking, skill: "", since: now)
            ask.step = event.step
            ask.skill = event.skill
            asks[event.ticket] = ask
        case .replied, .failed: asks[event.ticket] = nil
        default: break
        }
    }
}

extension Aria_Daemon_V1_ChatStep {
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

/// The enrolled voice heard in the press on screen. Each press starts
/// unheard, even one that fails before it records. An event naming no voice
/// keeps the last, so the ring does not drop out mid-press.
struct HeardVoice {
    private(set) var name: String?
    private var ended = false

    mutating func apply(_ event: Aria_Daemon_V1_DictationEvent) {
        if ended {
            name = nil
            ended = false
        }
        switch event.stage {
        case .recording: name = nil
        case .segmentTranscribed where !event.speaker.isEmpty: name = event.speaker
        case .delivered, .cancelled, .failed: ended = true
        default: break
        }
    }
}

private struct Pill: Equatable {
    let state: PillState
    let prompt: String?
    /// The color of the voice heard; none until one is recognized.
    let voice: VoiceColor?

    var label: String? { state.label(prompt: prompt?.shortenedForPill) }
}

enum OverlayStyle {
    static let fill = Color.black.opacity(0.75)
    static let font = Font.system(size: 12, weight: .medium)
    static let ringWidth: CGFloat = 1
}

extension String {
    /// A name short enough to leave room in the pill for the step and the
    /// clock.
    var shortenedForPill: String {
        let length = 20
        guard count > length else { return self }
        return prefix(length - 1).trimmingCharacters(in: .whitespaces) + "…"
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
    private let prompts: PromptHotkeys
    private let colors: VoiceColors
    private var heard = HeardVoice()
    private let panel = FloatingPanel(width: pillWidth, bottomMargin: pillBottomMargin)
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
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.enabledByDefault])
        events.onDictation { [weak self] event in
            guard let self else { return }
            guard let event else {
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
        let voice: VoiceColor? =
            switch foregroundRun {
            case .ticket: nil
            case .recording, nil: foregroundState == nil ? nil : heard.name.map(colors.assignColor(to:))
            }
        let wanted =
            enabled
            ? live.map { Pill(state: $0, prompt: run.flatMap(prompts.runs.prompt(of:)), voice: voice) } : nil
        guard wanted != shown else { return }
        shown = wanted
        // A new press starts from silence rather than the last one's level.
        if case .recording = wanted?.state { level.value = 0 }
        draw(wanted)
    }

    private func draw(_ pill: Pill?) {
        if let pill {
            panel.show(StatusPillView(pill: pill, level: level))
        } else {
            panel.hide()
        }
    }
}

/// Room for the longest failure message; the capsule centres in it.
let pillWidth: CGFloat = 480
/// The ribbon and the bar share one slot, so a press never jumps as the
/// mic gives way to the machine. A glyph is its own size, as on the board.
private let markWidth = RibbonTuning.width
private let markHeight = RibbonTuning.height
let pillBottomMargin: CGFloat = 34

/// A mark and, where there is something to read, a word for it.
private struct StatusPillView: View {
    let pill: Pill
    let level: LiveLevel

    var body: some View {
        let state = pill.state
        let label = pill.label
        HStack(spacing: 7) {
            Mark(state: state, level: level)
            if label != nil || state.since != nil {
                HStack(spacing: 4) {
                    if let label {
                        Text(label)
                    }
                    if let since = state.since {
                        if label != nil {
                            Text("·")
                        }
                        Text(since, style: .timer)
                    }
                }
                .font(OverlayStyle.font)
                .monospacedDigit()
                .foregroundStyle(.white)
                .padding(.trailing, 4)
            }
        }
        .padding(9)
        .background(OverlayStyle.fill, in: Capsule())
        .overlay {
            if let ring = pill.voice?.ring {
                Capsule().strokeBorder(ring, lineWidth: OverlayStyle.ringWidth)
            }
        }
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
        case .delivered(.copied, _), .replied(copied: true):
            Glyph(name: "doc.on.clipboard", tint: state.tint)
        case .delivered, .replied:
            Glyph(name: "checkmark", tint: state.tint)
        case .cancelled:
            Glyph(name: "xmark", tint: state.tint)
        case .interrupted:
            Glyph(name: "hand.raised", tint: state.tint)
        case .excluded:
            Glyph(name: "nosign", tint: state.tint)
        case .asking:
            // The symbol Mac apps use for work an AI model is doing.
            Glyph(name: "sparkles", tint: state.tint)
                .modifier(Breathing(motion: state.motion))
        case .failed, .noReply:
            // Keyed on the motion: a new motion is a new dot with its own
            // loop, while a label change under the same motion keeps this one.
            Circle()
                .fill(state.tint)
                .frame(width: 10, height: 10)
                .modifier(Breathing(motion: state.motion))
                .id(state.motion)
        }
    }
}

/// One bar in the ribbon's slot, filling left to right while text goes in.
private struct Bar: View {
    let tint: Color

    @State private var running = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Capsule()
            .fill(.white.opacity(0.18))
            .frame(width: markWidth, height: 3)
            .overlay(alignment: .leading) {
                Capsule()
                    .fill(tint)
                    .frame(width: markWidth, height: 3)
                    .scaleEffect(x: running || reduceMotion ? 1 : 0.001, anchor: .leading)
            }
            .clipShape(Capsule())
            .animation(.easeOut(duration: 1.2).repeatForever(autoreverses: false), value: running)
            .onAppear { running = !reduceMotion }
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

private struct Breathing: ViewModifier {
    let motion: Motion

    @State private var swelling = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content
            .scaleEffect(swelling ? motion.scale : 1)
            .animation(motion.animation, value: swelling)
            .onAppear { swelling = motion != .still && !reduceMotion }
    }
}
