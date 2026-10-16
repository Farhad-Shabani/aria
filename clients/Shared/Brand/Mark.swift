// The Aria logo. Not licensed under GPL-3.0; see assets/brand/LICENSE.

import SwiftUI

/// Every number that shapes the logo as a record button, in its 128-point
/// box, and times its check. The ribbon's waves and its motion come from
/// `RibbonTuning`, shared with the Mac pill.
enum MarkTuning {
    // Shape: the logo, as in assets/brand.

    /// The part of the box the logo fills; the rest is margin.
    static let visible = CGRect(x: 16.5, y: 16.5, width: 95, height: 95)
    static let center = 64.0
    static let radius = 44.0
    static let ring = 7.0
    static let outer = radius + ring / 2
    /// Outline stroke around the ribbon. Rounds its ends into the ring.
    static let edge = 6.0
    /// Gap between the currents' baselines.
    static let spread = 10.0
    /// How far the ribbon sits below the ring's centre.
    static let shift = 7.0
    /// Segments per current.
    static let points = 40
    /// Wave height as drawn in the logo.
    static let restHeight = 5.0
    /// Wave height at full voice.
    static let loudHeight = 12.0

    // Done: the ribbon settles flat, bends into a check, then unbends.

    /// Seconds for the last wave to settle into a line.
    static let settle = 0.2
    /// Seconds for the line to bend into the check.
    static let bend = 0.38
    /// Seconds for the check to unbend into a line.
    static let unbend = 0.3
    /// Seconds for the resting wave to grow back out of the line.
    static let regrow = 0.4
    /// The line and the check, as left end, corner and right end.
    static let line = [CGPoint(x: 32.1, y: 71), CGPoint(x: 57.9, y: 71), CGPoint(x: 95.9, y: 71)]
    static let check = [CGPoint(x: 42.7, y: 65.2), CGPoint(x: 56.7, y: 78), CGPoint(x: 86.8, y: 50)]
    /// Stroke of the line, as thick as the ribbon it came from, and of the check.
    static let lineWidth = spread + edge
    static let checkWidth = 9.0
}

extension RibbonGeometry {
    /// The logo's ribbon: across the ring's inner chord, below its centre.
    static let mark: RibbonGeometry = {
        let inner = MarkTuning.radius - MarkTuning.ring / 2
        let half = (inner * inner - MarkTuning.shift * MarkTuning.shift).squareRoot()
        return RibbonGeometry(
            left: MarkTuning.center - half,
            width: 2 * half,
            middle: MarkTuning.center + MarkTuning.shift,
            spread: MarkTuning.spread,
            maxHeight: MarkTuning.loudHeight,
            points: MarkTuning.points
        )
    }()
}

/// What the mark is showing.
enum MarkStage: Equatable, Sendable {
    case rest
    case listening
    case transcribing
    case done
}

/// What to draw inside the ring.
enum MarkFigure {
    /// The two currents as one filled band.
    case ribbon(Path)
    /// The ribbon pulled into a stroked line: flat, a check, or between.
    case line([CGPoint], width: Double)
}

/// The mark's motion, as plain numbers: no view, no clock, no audio.
struct MarkMotion: Equatable, Sendable {
    static let restShare = MarkTuning.restHeight / MarkTuning.loudHeight

    private(set) var stage = MarkStage.rest
    private(set) var ribbon = RibbonMotion(height: restShare)
    /// Seconds in the current stage.
    private(set) var elapsed = 0.0
    /// Whether this rest follows a delivery, so the check unbends first.
    private(set) var returning = false

    /// Moves `dt` seconds on, in `stage`, hearing the voice at `level`.
    mutating func advance(_ stage: MarkStage, level: Double, dt: Double) {
        if stage != self.stage {
            enter(stage)
        }
        elapsed += dt
        switch stage {
        case .listening:
            ribbon.listen(to: level, for: dt)
        case .transcribing:
            ribbon.transcribe(for: dt)
        case .rest, .done:
            break
        }
    }

    private mutating func enter(_ next: MarkStage) {
        returning = stage == .done && next == .rest
        if next == .listening {
            // A new press starts from the resting wave, not the last press.
            ribbon = RibbonMotion(height: Self.restShare)
        }
        stage = next
        elapsed = 0
    }

    var figure: MarkFigure {
        switch stage {
        case .listening, .transcribing:
            return Self.ribbonFigure(phase: ribbon.phase, share: ribbon.share(at:))
        case .done:
            if elapsed < MarkTuning.settle {
                let fade = 1 - smoothstep(0, MarkTuning.settle, elapsed)
                return Self.ribbonFigure(phase: ribbon.phase) { ribbon.share(at: $0) * fade }
            }
            return Self.bent(easeOutCubic((elapsed - MarkTuning.settle) / MarkTuning.bend))
        case .rest:
            guard returning else { return Self.resting }
            if elapsed < MarkTuning.unbend {
                return Self.bent(easeOutCubic(1 - elapsed / MarkTuning.unbend))
            }
            let grown = easeOutCubic((elapsed - MarkTuning.unbend) / MarkTuning.regrow)
            return Self.ribbonFigure(phase: 0) { _ in Self.restShare * grown }
        }
    }

    /// The logo as drawn.
    static var resting: MarkFigure {
        ribbonFigure(phase: 0) { _ in restShare }
    }

    /// The still mark for Reduce Motion: the logo, or the check once delivered.
    static func still(_ stage: MarkStage) -> MarkFigure {
        stage == .done ? .line(MarkTuning.check, width: MarkTuning.checkWidth) : resting
    }

    private static func ribbonFigure(phase: Double, share: (Double) -> Double) -> MarkFigure {
        .ribbon(RibbonMotion.outline(phase: phase, share: share, geometry: .mark))
    }

    /// The line bent `k` of the way into the check: points and stroke both move.
    static func bent(_ k: Double) -> MarkFigure {
        let points = zip(MarkTuning.line, MarkTuning.check).map { from, to in
            CGPoint(x: mix(from.x, to.x, k), y: mix(from.y, to.y, k))
        }
        return .line(points, width: mix(MarkTuning.lineWidth, MarkTuning.checkWidth, k))
    }
}

/// The logo: the ring and its figure, drawn in the foreground style.
struct LogoMark: View {
    let figure: MarkFigure

    var body: some View {
        Canvas { context, size in
            let scale = size.width / MarkTuning.visible.width
            context.scaleBy(x: scale, y: scale)
            context.translateBy(x: -MarkTuning.visible.minX, y: -MarkTuning.visible.minY)
            let ring = Path(
                ellipseIn: CGRect(
                    x: MarkTuning.center - MarkTuning.radius,
                    y: MarkTuning.center - MarkTuning.radius,
                    width: 2 * MarkTuning.radius,
                    height: 2 * MarkTuning.radius
                )
            )
            context.stroke(ring, with: .foreground, lineWidth: MarkTuning.ring)
            switch figure {
            case .ribbon(let band):
                context.fill(band, with: .foreground)
                context.stroke(
                    band, with: .foreground, style: StrokeStyle(lineWidth: MarkTuning.edge, lineJoin: .round))
            case .line(let points, let width):
                let line = Path { $0.addLines(points) }
                context.stroke(
                    line,
                    with: .foreground,
                    style: StrokeStyle(lineWidth: width, lineCap: .round, lineJoin: .round)
                )
            }
        }
        .aspectRatio(1, contentMode: .fit)
    }
}

func easeOutCubic(_ x: Double) -> Double {
    let k = clamp(x, 0, 1)
    return 1 - (1 - k) * (1 - k) * (1 - k)
}
