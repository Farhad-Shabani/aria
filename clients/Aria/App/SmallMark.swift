// The Aria logo. Not licensed under GPL-3.0; see assets/brand/LICENSE.

import SwiftUI

/// The logo at 32 points and below, as paths in its 128-point box. A ring of 7
/// reads too thin there, so the ring is heavier and the ribbon shrinks to its opening.
enum SmallMark {
    static let ringWidth = 12.0
    fileprivate static let inner = MarkTuning.outer - ringWidth
    fileprivate static let scale = inner / (MarkTuning.radius - MarkTuning.ring / 2)
    /// The currents sit further apart and the edge is heavier, so the ribbon
    /// holds its weight against the heavier ring.
    fileprivate static let spreadGain = 1.5
    private static let edgeGain = 1.25
    static let ribbonEdge = MarkTuning.edge * scale * edgeGain
    static let checkWidth = 10.0

    /// The ring's centre line.
    static let ring = circle(radius: MarkTuning.outer - ringWidth / 2)
    static let disc = circle(radius: MarkTuning.outer)

    /// The logo's resting ribbon. Fill it, then stroke it at `ribbonEdge`.
    static let ribbon = RibbonMotion.outline(phase: 0, share: { _ in MarkMotion.restShare }, geometry: .smallMark)

    /// One of three dots across the opening, `index` −1, 0 or 1.
    static func dot(_ index: Int) -> Path {
        let dotRadius = 7.0
        let gap = 18.0
        return circle(radius: dotRadius, at: CGPoint(x: center.x + Double(index) * gap, y: center.y))
    }

    static let check = Path { path in
        path.addLines(MarkTuning.check.map(scaled))
    }

    /// The gap clears around the badge so it never touches the ring, even at 1×.
    static let badge = circle(radius: 12, at: badgeCenter)
    static let badgeGap = circle(radius: 19, at: badgeCenter)
    private static let badgeCenter = CGPoint(x: 101, y: 27)
    private static let center = CGPoint(x: MarkTuning.center, y: MarkTuning.center)

    private static func scaled(_ point: CGPoint) -> CGPoint {
        CGPoint(x: center.x + (point.x - center.x) * scale, y: center.y + (point.y - center.y) * scale)
    }

    private static func circle(radius: Double, at point: CGPoint = center) -> Path {
        Path(ellipseIn: CGRect(x: point.x - radius, y: point.y - radius, width: 2 * radius, height: 2 * radius))
    }
}

extension RibbonGeometry {
    /// The logo's ribbon inside the small ring, scaled to its opening.
    static let smallMark: RibbonGeometry = {
        let shift = MarkTuning.shift * SmallMark.scale
        let half = (SmallMark.inner * SmallMark.inner - shift * shift).squareRoot()
        return RibbonGeometry(
            left: MarkTuning.center - half,
            width: 2 * half,
            middle: MarkTuning.center + shift,
            spread: MarkTuning.spread * SmallMark.scale * SmallMark.spreadGain,
            maxHeight: MarkTuning.loudHeight * SmallMark.scale,
            segmentCount: MarkTuning.segmentCount
        )
    }()
}
