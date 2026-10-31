// The Aria logo. Not licensed under GPL-3.0; see assets/brand/LICENSE.

import AppKit
import SwiftUI

enum MenuBarMark {
    /// Menu bar canvas and the ring's outer diameter inside it, in points.
    private static let canvas: CGFloat = 18
    private static let ringSize: CGFloat = 16
    private static let switchingAlpha: CGFloat = 0.5
    private static let stoppedAlpha: CGFloat = 0.45

    static func image(_ state: MenuBarState) -> NSImage {
        let image = NSImage(size: NSSize(width: canvas, height: canvas), flipped: true) { _ in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            let scale = ringSize / (2 * MarkTuning.outer)
            context.translateBy(x: canvas / 2 - MarkTuning.center * scale, y: canvas / 2 - MarkTuning.center * scale)
            context.scaleBy(x: scale, y: scale)

            // One layer, so knockouts clear only the mark and dimmed strokes don't double up.
            context.setAlpha(alpha(state))
            context.beginTransparencyLayer(auxiliaryInfo: nil)
            defer { context.endTransparencyLayer() }
            context.setFillColor(NSColor.black.cgColor)
            context.setStrokeColor(NSColor.black.cgColor)
            context.setLineCap(.round)
            context.setLineJoin(.round)
            draw(state, in: context)
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = state.accessibilityLabel
        return image
    }

    private static func alpha(_ state: MenuBarState) -> CGFloat {
        switch state {
        case .switching: return switchingAlpha
        case .stopped: return stoppedAlpha
        case .ready, .listening, .working, .done, .attention, .updateAvailable: return 1
        }
    }

    private static func draw(_ state: MenuBarState, in context: CGContext) {
        switch state {
        case .ready, .switching, .stopped:
            ringAndRibbon(in: context)
        case .listening:
            // Inverted, so a live microphone is unmistakable at a glance.
            fill(SmallMark.disc, in: context)
            context.setBlendMode(.clear)
            ribbon(in: context)
            context.setBlendMode(.normal)
        case .working:
            ring(in: context)
            for index in -1...1 { fill(SmallMark.dot(index), in: context) }
        case .done:
            ring(in: context)
            stroke(SmallMark.check, width: SmallMark.checkWidth, in: context)
        case .attention, .updateAvailable:
            ringAndRibbon(in: context)
            context.setBlendMode(.clear)
            fill(SmallMark.badgeGap, in: context)
            context.setBlendMode(.normal)
            fill(SmallMark.badge, in: context)
        }
    }

    private static func ringAndRibbon(in context: CGContext) {
        ring(in: context)
        ribbon(in: context)
    }

    private static func ring(in context: CGContext) {
        stroke(SmallMark.ring, width: SmallMark.ringWidth, in: context)
    }

    private static func ribbon(in context: CGContext) {
        let ribbon = SmallMark.ribbon
        fill(ribbon, in: context)
        stroke(ribbon, width: SmallMark.ribbonEdge, in: context)
    }

    private static func fill(_ path: Path, in context: CGContext) {
        context.addPath(path.cgPath)
        context.fillPath()
    }

    private static func stroke(_ path: Path, width: Double, in context: CGContext) {
        context.setLineWidth(width)
        context.addPath(path.cgPath)
        context.strokePath()
    }
}
