import SwiftUI

/// The tide's shape and color, in points.
private enum TideTuning {
    static let height: CGFloat = 56
    /// How high the tide settles in silence, from the bottom: inside
    /// the footer, so its surface never lines up with the footer's top and
    /// reads as a border.
    static let restingHeight = 24.0
    /// How far loud audio lifts the line.
    static let lift = 6.0
    /// Gap between the two currents' baselines.
    static let spread = 6.0
    /// Wave height at full share. Low, so sensitivity reads as motion, not height.
    static let maxHeight = 8.0
    /// The share the waves keep in silence, so the surface is never a ruled line.
    static let restShare = 0.2
    static let segmentCount = 48
    /// The site's glow mixed into its paper. Primary text reads about 7 : 1 on it.
    static let fill = Color(red: 0.204, green: 0.306, blue: 0.278)
    /// The tide's opacity in silence and at full height.
    static let quietOpacity = 0.5
    /// Edge blur in silence and at full height: a soft haze that firms with sound.
    static let quietBlur = 3.0
    static let loudBlur = 1.4
    /// Seconds for the loudest recent level to fade. The tide follows the audio
    /// against it, so quiet audio moves it as much as loud.
    static let peakFade = 3.0
    /// The quietest peak the tide scales to, so silence stays still.
    static let peakFloor: Float = 0.08
    /// The palette needs no more: the motion reads the same at half the display's rate.
    static let frameInterval = 1.0 / 30
}

/// Steps the motion once per frame. Not observable on purpose, as `RibbonModel`.
@MainActor
private final class TideModel {
    private(set) var motion = RibbonMotion()
    private var lastFrame: Date?
    private var peak = TideTuning.peakFloor

    func step(at date: Date, level: Float) -> RibbonMotion {
        let dt = lastFrame.map { date.timeIntervalSince($0) } ?? 0
        lastFrame = date
        let fade = Float(exp(-dt / TideTuning.peakFade))
        peak = max(level, peak * fade, TideTuning.peakFloor)
        motion.listen(to: Double(level / peak), for: dt)
        return motion
    }
}

/// A soft tide that rises and flows with a level, in the logo's two currents. The
/// whole effect lives in this file and needs only the level, so another can
/// take its place in `PaletteAudioBackdrop`.
struct PaletteTide: View {
    let level: LiveLevel

    @State private var model = TideModel()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(minimumInterval: TideTuning.frameInterval)) { context in
            let motion = model.step(at: context.date, level: level.value)
            Canvas { canvas, size in
                draw(motion, in: &canvas, size: size)
            }
        }
        .frame(height: TideTuning.height)
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }

    private func draw(_ motion: RibbonMotion, in canvas: inout GraphicsContext, size: CGSize) {
        let energy = min(max(motion.height, 0), 1)
        let geometry = RibbonGeometry(
            left: 0,
            width: size.width,
            middle: size.height - TideTuning.restingHeight - TideTuning.lift * energy,
            spread: TideTuning.spread,
            maxHeight: TideTuning.maxHeight,
            segmentCount: TideTuning.segmentCount
        )
        // Under Reduce Motion the tide still rises and falls with the audio, but doesn't flow.
        let phase = reduceMotion ? 0 : motion.phase
        let (back, front) = RibbonMotion.currents(
            phase: phase,
            share: { max(motion.share(at: $0), TideTuning.restShare) },
            geometry: geometry)
        let top = geometry.middle - geometry.spread - geometry.maxHeight
        let shading = GraphicsContext.Shading.linearGradient(
            Gradient(colors: [TideTuning.fill.opacity(0.7), TideTuning.fill]),
            startPoint: CGPoint(x: 0, y: top),
            endPoint: CGPoint(x: 0, y: size.height)
        )
        canvas.opacity = TideTuning.quietOpacity + (1 - TideTuning.quietOpacity) * energy
        canvas.addFilter(
            .blur(radius: TideTuning.quietBlur + (TideTuning.loudBlur - TideTuning.quietBlur) * energy))
        canvas.drawLayer { layer in
            layer.opacity = 0.5
            layer.fill(filled(back, down: size), with: shading)
        }
        canvas.fill(filled(front, down: size), with: shading)
    }

    /// A current closed down to the bottom edge.
    private func filled(_ current: [CGPoint], down size: CGSize) -> Path {
        Path { path in
            path.addLines(current)
            path.addLine(to: CGPoint(x: size.width, y: size.height))
            path.addLine(to: CGPoint(x: 0, y: size.height))
            path.closeSubpath()
        }
    }
}
