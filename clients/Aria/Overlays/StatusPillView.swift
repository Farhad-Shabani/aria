import SwiftUI

struct Pill: Equatable {
    let state: PillState
    let prompt: String?
    let voice: VoiceEmphasis?
    let glows: Bool

    var label: String? { state.label(prompt: prompt) }
}

extension Pill {
    /// Only Aria's own work glows: an ask, or a ticket of no prompt.
    /// Dictation and prompts do not. `ask` is the last recording an ask
    /// press started, and `prompt` the prompt behind the run.
    static func glows(run: RunID?, ask: RunID?, prompt: String?) -> Bool {
        guard prompt == nil else { return false }
        switch run {
        case .ticket: return true
        case .recording: return run == ask
        case nil: return false
        }
    }
}

/// How the waves show the voice heard: brighter while its words are typed,
/// faint while they are kept out.
enum VoiceEmphasis: Equatable {
    case typed
    case keptOut

    init(_ voice: HeardSpeaker.Voice) {
        switch voice {
        case .named: self = .typed
        case .keptOut: self = .keptOut
        }
    }
}

/// The ribbon and the bar share one slot, so a press never jumps as the
/// mic gives way to the machine. A glyph is its own size, as on the board.
private let markWidth = RibbonTuning.width
private let markHeight = RibbonTuning.height

/// A mark and, where there is something to read, a word for it.
struct StatusPillView: View {
    let pill: Pill
    let level: LiveLevel

    var body: some View {
        let state = pill.state
        let label = pill.label
        HStack(spacing: 7) {
            Mark(state: state, level: level, voice: pill.voice)
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
                .font(OverlayStyle.labelFont)
                .monospacedDigit()
                .foregroundStyle(OverlayStyle.ink)
                .padding(.trailing, 4)
            }
        }
        .padding(9)
        .background { Frost.capsule(floor: FloorGlow(isLit: pill.glows)) }
    }
}

/// The glow's shape and motion. Heights are shares of the pill's height,
/// widths of its width.
private enum GlowTuning {
    /// How far up the glow reaches before it fades out.
    static let reach = 0.38
    /// How far out to each side it reaches from the middle of the floor.
    static let spread = 0.75
    /// Brightness and lift at the top of the rise, over their resting 1.
    static let peakBrightness = 1.35
    static let peakLift = 1.15
    /// Seconds to rise, to settle back to rest, and to go out.
    static let riseTime = 0.6
    static let settleTime = 2.0
    static let fadeTime = 0.3
}

private struct GlowFrame {
    var brightness: Double
    var lift: Double
}

/// Light rising from the pill's floor while Aria works on an ask. It lifts
/// past rest and settles, so it draws the eye once and then stays quiet. An
/// ask is known a beat after the pill appears, so its glow rises in late.
private struct FloorGlow: View {
    let isLit: Bool

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        // Reduce Motion fades the glow in and out without rising or sinking.
        let rest = GlowFrame(brightness: 1, lift: 1)
        let peak = reduceMotion ? rest : GlowFrame(brightness: GlowTuning.peakBrightness, lift: GlowTuning.peakLift)
        let out = GlowFrame(brightness: 0, lift: reduceMotion ? 1 : 0)
        KeyframeAnimator(initialValue: isLit ? rest : out, trigger: isLit) { frame in
            // A fraction of 1 spans the whole shape, so `reach` is the share
            // of the height. Dividing by it in the x scale leaves `spread` of the width.
            EllipticalGradient(
                colors: [
                    OverlayStyle.askGlow.opacity(OverlayStyle.askGlowOpacity * frame.brightness),
                    OverlayStyle.askGlow.opacity(0),
                ],
                center: .bottom,
                endRadiusFraction: GlowTuning.reach
            )
            .scaleEffect(x: GlowTuning.spread / GlowTuning.reach, y: frame.lift, anchor: .bottom)
        } keyframes: { _ in
            KeyframeTrack(\.brightness) {
                if isLit {
                    CubicKeyframe(peak.brightness, duration: GlowTuning.riseTime)
                    CubicKeyframe(rest.brightness, duration: GlowTuning.settleTime)
                } else {
                    CubicKeyframe(out.brightness, duration: GlowTuning.fadeTime)
                }
            }
            KeyframeTrack(\.lift) {
                if isLit {
                    CubicKeyframe(peak.lift, duration: GlowTuning.riseTime)
                    CubicKeyframe(rest.lift, duration: GlowTuning.settleTime)
                } else {
                    CubicKeyframe(out.lift, duration: GlowTuning.fadeTime)
                }
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
    let voice: VoiceEmphasis?

    var body: some View {
        switch state {
        case .recording, .transcribing:
            // One branch, so the ribbon's motion carries across the hand-off.
            Ribbon(state: state, level: level, voice: voice)
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

    @State private var isRunning = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Capsule()
            .fill(.white.opacity(0.18))
            .frame(width: markWidth, height: 3)
            .overlay(alignment: .leading) {
                Capsule()
                    .fill(tint)
                    .frame(width: markWidth, height: 3)
                    .scaleEffect(x: isRunning || reduceMotion ? 1 : 0.001, anchor: .leading)
            }
            .clipShape(Capsule())
            .animation(.easeOut(duration: 1.2).repeatForever(autoreverses: false), value: isRunning)
            .onAppear { isRunning = !reduceMotion }
    }
}

private struct Glyph: View {
    let name: String
    let tint: Color

    var body: some View {
        Image(systemName: name)
            .font(OverlayStyle.glyphFont)
            .foregroundStyle(tint)
            .frame(width: markHeight, height: markHeight)
    }
}

private struct Breathing: ViewModifier {
    let motion: Motion

    @State private var isSwelling = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content
            .scaleEffect(isSwelling ? motion.scale : 1)
            .animation(motion.animation, value: isSwelling)
            .onAppear { isSwelling = motion != .still && !reduceMotion }
    }
}
