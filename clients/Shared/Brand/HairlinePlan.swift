// The Aria logo. Not licensed under GPL-3.0; see assets/brand/LICENSE.

import CoreGraphics

/// The hairline mark's lines in the logo's units, planned in whole device pixels. On a mark an even
/// number of device pixels wide, ring edges land on pixel edges, so a ring stays crisp where it runs
/// along the pixel grid.
struct HairlinePlan: Equatable, Sendable {
    let hairline: Double
    /// From one line's centre to the next: the rings' spacing, and the closest the band's lines come.
    let step: Double
    /// Each ring's radius, outermost first.
    let rings: [Double]
    /// The radius of the opening inside the rings.
    let opening: Double
    let gaps: Int

    /// As the website sets them: the thinnest line in device pixels, and the mark's width in device
    /// pixels per pixel of line.
    private static let minLine = 1.0
    private static let pixelsPerLine = 400.0
    /// Fewer gaps across the ring than the website's.
    private static let ringGaps = 9.0
    /// In device pixels. A one-pixel line that moves between pixels blurs to gray.
    private static let minPixels = 2.0
    private static let minRings = 2
    /// Where more would fit, the band stops here, so a large mark stays light.
    private static let maxWaves = 16

    /// The band at its narrowest at rest, edge included. Its lines fit there a step apart.
    private static let narrowest: Double = {
        guard case .ribbon(let upper, let lower) = MarkMotion.resting else {
            preconditionFailure("the resting mark is a ribbon")
        }
        return zip(upper, lower).map { $1.y - $0.y }.reduce(.infinity, min) + MarkTuning.edge
    }()

    /// How heavy the lines are, and how close they may run.
    struct Lines: Equatable, Sendable {
        /// Line weight against the website's.
        let weight: Double
        /// In device pixels.
        let minGap: Double
        var ringFit = RingFit.overhang

        /// For a mark in the app: heavier than the website's, never closer than 2 px.
        static let button = Lines(weight: 1.4, minGap: 2)
        /// For a small mark, such as the app icon's or the pairing code's, which needs more light:
        /// twice the website's weight, 1 px apart.
        static let small = Lines(weight: 2, minGap: 1)
        /// For a large mark: heavier lines than the button's with the same 2 px gaps, so fewer rings
        /// and waves read calmer but no dimmer. A 176 pt mark at @3x draws 8 rings · 14 waves.
        static let large = Lines(weight: 2.2, minGap: 2)
    }

    /// Which rings the ring's thickness holds. Two always draw, even where they run past it.
    enum RingFit: Equatable, Sendable {
        /// Every ring that starts inside it, so the last may run past it.
        case overhang
        /// Only rings that end inside it, so the ring stays as thick as the solid logo's.
        case inside
    }

    init(width: Double, displayScale: Double, lines: Lines = .button) {
        let perUnit = width / MarkTuning.visible.width * displayScale
        let pixel = 1 / perUnit
        let ringPixels = MarkTuning.ring * perUnit
        func fit(_ linePixels: Double) -> (step: Double, count: Int) {
            let step = max((MarkTuning.ring / Self.ringGaps * perUnit).rounded(), linePixels + lines.minGap)
            let span =
                switch lines.ringFit {
                case .overhang: ringPixels
                case .inside: ringPixels - linePixels
                }
            return (step, Int((span / step).rounded(.down)) + 1)
        }
        var linePixels = max(
            (max(Self.minLine, width * displayScale / Self.pixelsPerLine) * lines.weight).rounded(), Self.minPixels)
        var (stepPixels, count) = fit(linePixels)
        // One ring reads as a plain outline, not the engraved mark, so a small mark thins its lines
        // to fit two, and keeps two even where they run past the ring.
        if count < Self.minRings {
            linePixels = Self.minLine
            (stepPixels, count) = fit(linePixels)
            count = max(count, Self.minRings)
        }
        let outerPixels = (MarkTuning.outer * perUnit).rounded()
        hairline = linePixels * pixel
        step = stepPixels * pixel
        rings = (0..<count).map { (outerPixels - linePixels / 2 - Double($0) * stepPixels) * pixel }
        opening = rings[count - 1] - 1.5 * hairline
        gaps = min(max(Int((Self.narrowest / step).rounded(.down)), 2), Self.maxWaves - 1)
    }

    /// Where the currents pinch closer than at rest, the lines stay a step apart about the middle.
    /// Packed tighter, they merge into moiré.
    func bandLines(upper: [CGPoint], lower: [CGPoint]) -> [[CGPoint]] {
        let count = Double(gaps)
        return (0...gaps).map { i in
            let offset = Double(i) - count / 2
            return zip(upper, lower).map { u, l in
                let pitch = max(step, (l.y - u.y + MarkTuning.edge) / count)
                return CGPoint(x: u.x, y: (u.y + l.y) / 2 + offset * pitch)
            }
        }
    }

    /// The rows stay put while the shape bends over them. Across the flat line they are the flat
    /// ribbon's lines, so the mark doesn't jump as the ribbon becomes the line.
    func rows(from top: Double, through bottom: Double) -> [Double] {
        let pitch = max(step, (MarkTuning.spread + MarkTuning.edge) / Double(gaps))
        let middle = RibbonGeometry.mark.middle + Double(gaps % 2) * pitch / 2
        // Out to the clip's edge, half a line past the stroke, so rows on the edge survive rounding.
        let first = ((top - hairline / 2 - middle) / pitch).rounded(.up)
        let last = ((bottom + hairline / 2 - middle) / pitch).rounded(.down)
        return stride(from: first, through: last, by: 1).map { middle + $0 * pitch }
    }
}
