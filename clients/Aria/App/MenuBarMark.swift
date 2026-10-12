// The Aria logo. Not licensed under AGPL-3.0; see assets/brand/LICENSE.

import AppKit

/// The logo drawn as a menu bar template image: a ring holding the ribbon.
enum MenuBarMark {
    // The logo, in its 128-unit box with y down, as the phone and Watch draw it.
    private static let center = CGFloat(MarkTuning.center)
    private static let radius = CGFloat(MarkTuning.radius)
    private static let ringStroke = CGFloat(MarkTuning.ring)
    private static let edgeStroke = CGFloat(MarkTuning.edge)
    private static let periods = CGFloat(RibbonTuning.periods)
    private static let amplitude = CGFloat(MarkTuning.restHeight)
    private static let spread = CGFloat(MarkTuning.spread)
    private static let shift = CGFloat(MarkTuning.shift)
    private static let phaseGap = CGFloat(RibbonTuning.phaseGap)
    private static let points = 36

    /// Menu bar canvas and the ring's outer diameter inside it, in points.
    private static let canvas: CGFloat = 18
    private static let ringSize: CGFloat = 16

    /// `wave` scales the ribbon's height: 1 is the logo, 0 is a flat band.
    static func image(wave: CGFloat, alpha: CGFloat) -> NSImage {
        let image = NSImage(size: NSSize(width: canvas, height: canvas), flipped: true) { _ in
            let scale = ringSize / (2 * radius + ringStroke)
            let transform = NSAffineTransform()
            transform.translateX(by: canvas / 2 - center * scale, yBy: canvas / 2 - center * scale)
            transform.scale(by: scale)
            transform.concat()

            // One layer, so the ribbon's fill and edge don't double up when dimmed.
            let context = NSGraphicsContext.current?.cgContext
            context?.setAlpha(alpha)
            context?.beginTransparencyLayer(auxiliaryInfo: nil)
            defer { context?.endTransparencyLayer() }
            NSColor.black.set()
            let ring = NSBezierPath(
                ovalIn: NSRect(x: center - radius, y: center - radius, width: 2 * radius, height: 2 * radius))
            ring.lineWidth = ringStroke
            ring.stroke()

            let ribbon = ribbon(wave: wave)
            ribbon.lineWidth = edgeStroke
            ribbon.lineJoinStyle = .round
            ribbon.fill()
            ribbon.stroke()
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = "Aria"
        return image
    }

    private static func ribbon(wave: CGFloat) -> NSBezierPath {
        let outline = current(0, wave: wave) + current(1, wave: wave).reversed()
        let path = NSBezierPath()
        path.move(to: outline[0])
        for point in outline.dropFirst() { path.line(to: point) }
        path.close()
        return path
    }

    /// One current, spanning the ring's inner edge at its baseline.
    private static func current(_ index: Int, wave: CGFloat) -> [CGPoint] {
        let side: CGFloat = index == 0 ? -1 : 1
        let baseline = center + shift + side * spread / 2
        let inner = radius - ringStroke / 2
        let half = (inner * inner - (baseline - center) * (baseline - center)).squareRoot()
        return (0...points).map { step in
            let p = CGFloat(step) / CGFloat(points)
            let swing = sin(2 * .pi * periods * p + CGFloat(index) * phaseGap)
            let y = baseline - wave * amplitude * sin(.pi * p) * swing
            return CGPoint(x: center - half + 2 * half * p, y: y)
        }
    }
}
