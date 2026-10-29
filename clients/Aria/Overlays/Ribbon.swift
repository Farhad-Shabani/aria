import SwiftUI

/// The microphone's latest level, 0 to 1. The ribbon reads it every frame,
/// so a new level needs no redraw of the pill around it.
@MainActor
final class LiveLevel {
    var value: Float = 0
}

/// Steps the motion once per frame. Not observable on purpose: it changes
/// while the view draws, and observing it would redraw in a loop.
@MainActor
final class RibbonModel {
    private(set) var motion = RibbonMotion()
    private var lastFrame: Date?
    private var press: Date?

    func outline(at date: Date, state: PillState, level: Float) -> Path {
        if case .recording(let since) = state, since != press {
            // A new press starts from silence, not from the last one's motion.
            motion = RibbonMotion()
            press = since
            lastFrame = date
        }
        let dt = lastFrame.map { date.timeIntervalSince($0) } ?? 0
        lastFrame = date
        switch state {
        case .recording:
            motion.listen(to: Double(level), for: dt)
        case .transcribing:
            motion.transcribe(for: dt)
        default:
            break
        }
        return motion.outline
    }
}

/// The voice while recording and the sweep while transcribing, as one
/// ribbon, so its motion carries across from one stage to the next.
struct Ribbon: View {
    let state: PillState
    let level: LiveLevel
    /// Shown only while recording; the sweep after it is the machine's.
    let voice: VoiceEmphasis?

    @State private var model = RibbonModel()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if reduceMotion {
                draw(RibbonMotion.outline(phase: 0, share: { _ in RibbonTuning.stillShare }))
            } else {
                TimelineView(.animation) { context in
                    draw(model.outline(at: context.date, state: state, level: level.value))
                }
            }
        }
        .frame(width: RibbonTuning.width, height: RibbonTuning.height)
        .animation(.easeInOut(duration: 0.3), value: voice)
    }

    private var isRecording: Bool {
        if case .recording = state { true } else { false }
    }

    private func draw(_ outline: Path) -> some View {
        let isTyped = isRecording && voice == .typed
        let tint = isTyped ? OverlayStyle.typedVoiceTint : state.tint
        let opacity =
            if !isRecording {
                1.0
            } else {
                switch voice {
                case .typed: 1.0
                case .keptOut: OverlayStyle.keptOutVoiceOpacity
                case nil: OverlayStyle.unjudgedVoiceOpacity
                }
            }
        return
            outline
            .fill(tint)
            .stroke(tint, style: StrokeStyle(lineWidth: RibbonTuning.lineWidth, lineJoin: .round))
            .shadow(color: isTyped ? OverlayStyle.typedVoiceHalo : .clear, radius: OverlayStyle.typedVoiceHaloRadius)
            .opacity(opacity)
    }
}
