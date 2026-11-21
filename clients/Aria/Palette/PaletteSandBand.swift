import SwiftUI

/// The band's shape and color. Heights are in points from the band's top.
private enum SandBandTuning {
    static let height: CGFloat = 90
    /// Where the sand settles in silence: deep enough to sit behind the footer.
    static let restingLine = 54.0
    /// How far loud audio lifts the line.
    static let lift = 8.0
    /// Gap between the two currents' baselines.
    static let spread = 7.0
    /// Wave height at full share. Low, so sensitivity reads as motion, not height.
    static let maxHeight = 12.0
    static let segmentCount = 48
    /// The site's glow mixed into its paper. Primary text reads about 7 : 1 on it.
    static let sand = Color(red: 0.204, green: 0.306, blue: 0.278)
    /// The sand's opacity in silence and at full height.
    static let quietOpacity = 0.5
    /// Edge blur in silence and at full height: a soft haze that firms with sound.
    static let quietBlur = 3.0
    static let loudBlur = 1.4
    /// Seconds for the loudest recent level to fade. The band follows the audio
    /// against it, so quiet audio moves it as much as loud.
    static let peakFade = 3.0
    /// The quietest peak the band scales to, so silence stays still.
    static let peakFloor: Float = 0.08
    /// The palette needs no more: the motion reads the same at half the display's rate.
    static let frameInterval = 1.0 / 30
}

/// Steps the motion once per frame. Not observable on purpose, as `RibbonModel`.
@MainActor
private final class SandBandModel {
    private(set) var motion = RibbonMotion()
    private var lastFrame: Date?
    private var peak = SandBandTuning.peakFloor

    func step(at date: Date, level: Float) -> RibbonMotion {
        let dt = lastFrame.map { date.timeIntervalSince($0) } ?? 0
        lastFrame = date
        let fade = Float(exp(-dt / SandBandTuning.peakFade))
        peak = max(level, peak * fade, SandBandTuning.peakFloor)
        motion.listen(to: Double(level / peak), for: dt)
        return motion
    }
}

/// Sand behind the footer while Live Transcript runs, moving with the Mac's
/// audio in the logo's two currents.
struct PaletteSandBand: View {
    let level: LiveLevel

    @State private var model = SandBandModel()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(minimumInterval: SandBandTuning.frameInterval)) { context in
            let motion = model.step(at: context.date, level: level.value)
            Canvas { canvas, size in
                draw(motion, in: &canvas, size: size)
            }
        }
        .frame(height: SandBandTuning.height)
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }

    private func draw(_ motion: RibbonMotion, in canvas: inout GraphicsContext, size: CGSize) {
        let energy = min(max(motion.height, 0), 1)
        let geometry = RibbonGeometry(
            left: 0,
            width: size.width,
            middle: SandBandTuning.restingLine - SandBandTuning.lift * energy,
            spread: SandBandTuning.spread,
            maxHeight: SandBandTuning.maxHeight,
            segmentCount: SandBandTuning.segmentCount
        )
        // Under Reduce Motion the sand still rises and falls with the audio, but doesn't flow.
        let phase = reduceMotion ? 0 : motion.phase
        let (back, front) = RibbonMotion.currents(phase: phase, share: motion.share(at:), geometry: geometry)
        let top = geometry.middle - geometry.spread - geometry.maxHeight
        let shading = GraphicsContext.Shading.linearGradient(
            Gradient(colors: [SandBandTuning.sand.opacity(0.7), SandBandTuning.sand]),
            startPoint: CGPoint(x: 0, y: top),
            endPoint: CGPoint(x: 0, y: size.height)
        )
        canvas.opacity = SandBandTuning.quietOpacity + (1 - SandBandTuning.quietOpacity) * energy
        canvas.addFilter(
            .blur(radius: SandBandTuning.quietBlur + (SandBandTuning.loudBlur - SandBandTuning.quietBlur) * energy))
        canvas.drawLayer { layer in
            layer.opacity = 0.5
            layer.fill(filled(back, down: size), with: shading)
        }
        canvas.fill(filled(front, down: size), with: shading)
    }

    /// A current closed down to the band's bottom edge.
    private func filled(_ current: [CGPoint], down size: CGSize) -> Path {
        Path { path in
            path.addLines(current)
            path.addLine(to: CGPoint(x: size.width, y: size.height))
            path.addLine(to: CGPoint(x: 0, y: size.height))
            path.closeSubpath()
        }
    }
}

extension EnvironmentValues {
    /// Whether the sand band sits behind the footer, so its text reads at full ink.
    @Entry var isOverSand = false
}
