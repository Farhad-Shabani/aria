// The Aria logo. Not licensed under GPL-3.0; see assets/brand/LICENSE.

import SwiftUI

/// The logo drawn in hairlines, as the website draws it.
struct HairlineMark: View {
    let figure: MarkFigure
    var lines = HairlinePlan.Lines.button
    @Environment(\.displayScale) private var displayScale

    /// The share of the opening's width over which the lines fade into the ring.
    private static let fade = 0.1

    var body: some View {
        Canvas { context, size in
            let scale = size.width / MarkTuning.visible.width
            context.scaleBy(x: scale, y: scale)
            context.translateBy(x: -MarkTuning.visible.minX, y: -MarkTuning.visible.minY)
            let plan = HairlinePlan(width: size.width, displayScale: displayScale, lines: lines)
            for radius in plan.rings {
                context.stroke(Self.circle(radius), with: .foreground, lineWidth: plan.hairline)
            }
            let opening = Self.circle(plan.opening)
            context.drawLayer { lines in
                lines.clip(to: opening)
                switch figure {
                case .ribbon(let upper, let lower):
                    for points in plan.bandLines(upper: upper, lower: lower) {
                        lines.stroke(Path { $0.addLines(points) }, with: .foreground, lineWidth: plan.hairline)
                    }
                case .line(let points, let width):
                    // Grown by a line, so the outer rows draw whole, as the flat ribbon's do.
                    let stroke = Path { $0.addLines(points) }
                    lines.clip(
                        to: stroke.strokedPath(
                            StrokeStyle(lineWidth: width + plan.hairline, lineCap: .round, lineJoin: .round)))
                    let ys = points.map(\.y)
                    let top = ys.reduce(.infinity, min) - width / 2
                    let bottom = ys.reduce(-.infinity, max) + width / 2
                    for y in plan.rows(from: top, through: bottom) {
                        let row = Path {
                            $0.move(to: CGPoint(x: MarkTuning.center - plan.opening, y: y))
                            $0.addLine(to: CGPoint(x: MarkTuning.center + plan.opening, y: y))
                        }
                        lines.stroke(row, with: .foreground, lineWidth: plan.hairline)
                    }
                }
                // Lines fade as they near the ring, so they arrive into it rather than hit it.
                lines.blendMode = .destinationIn
                lines.fill(
                    opening,
                    with: .radialGradient(
                        Gradient(stops: [
                            .init(color: .black, location: 1 - 2 * Self.fade), .init(color: .clear, location: 1),
                        ]),
                        center: CGPoint(x: MarkTuning.center, y: MarkTuning.center), startRadius: 0,
                        endRadius: plan.opening))
            }
        }
        .aspectRatio(1, contentMode: .fit)
        .accessibilityHidden(true)
    }

    private static func circle(_ radius: Double) -> Path {
        Path(
            ellipseIn: CGRect(
                x: MarkTuning.center - radius, y: MarkTuning.center - radius, width: 2 * radius, height: 2 * radius))
    }
}
