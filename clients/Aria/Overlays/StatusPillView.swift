import SwiftUI

struct Pill: Equatable {
    let state: PillState
    let prompt: String?
    /// The color of the voice heard; none until one is recognized.
    let voice: VoiceColor?

    var label: String? { state.label(prompt: prompt) }
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
            .font(.system(size: 10, weight: .bold))
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
