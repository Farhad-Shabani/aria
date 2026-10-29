import AppKit
import SwiftUI

/// Frosted glass behind an overlay: the windows under it blurred, a dark
/// fill, a faint top light and a lit edge, so the pane looks slightly
/// raised. A floor, such as a glow, sits between the fill and the light.
struct Frost: View {
    private let glass: AnyShape
    /// AppKit masks the blur and needs the corner radius as a number; a
    /// capsule's is half its height.
    private let maskRadius: (CGSize) -> CGFloat
    private let floor: AnyView

    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    static func capsule(floor: some View = EmptyView()) -> Frost {
        Frost(glass: AnyShape(Capsule()), maskRadius: { $0.height / 2 }, floor: AnyView(floor))
    }

    static func card(cornerRadius: CGFloat) -> Frost {
        Frost(
            glass: AnyShape(RoundedRectangle(cornerRadius: cornerRadius)),
            maskRadius: { _ in cornerRadius },
            floor: AnyView(EmptyView())
        )
    }

    var body: some View {
        glass
            .fill(OverlayStyle.frost)
            // Reduce Transparency turns the blur solid, so all of it shows.
            .background {
                BehindWindowBlur(maskRadius: maskRadius).opacity(reduceTransparency ? 1 : OverlayStyle.frostBlur)
            }
            .overlay { floor }
            .overlay {
                LinearGradient(colors: [OverlayStyle.frostSheen, .clear], startPoint: .top, endPoint: .center)
            }
            // Stroked at twice the width, then clipped, so only the inner
            // half shows: a border on a shape that cannot inset.
            .overlay { glass.stroke(OverlayStyle.frostEdge, lineWidth: OverlayStyle.frostEdgeWidth * 2) }
            .clipShape(glass)
    }
}

/// The windows behind the glass, blurred. An overlay is its own transparent
/// window, so only AppKit can blur what lies under it.
private struct BehindWindowBlur: NSViewRepresentable {
    let maskRadius: (CGSize) -> CGFloat

    func makeNSView(context: Context) -> GlassBlurView {
        let view = GlassBlurView()
        view.maskRadius = maskRadius
        view.material = .hudWindow
        view.blendingMode = .behindWindow
        view.state = .active
        view.appearance = NSAppearance(named: .darkAqua)
        return view
    }

    func updateNSView(_ view: GlassBlurView, context: Context) {}
}

private final class GlassBlurView: NSVisualEffectView {
    var maskRadius: (CGSize) -> CGFloat = { _ in 0 }

    override func layout() {
        super.layout()
        let radius = maskRadius(bounds.size)
        maskImage = NSImage(size: bounds.size, flipped: false) { rect in
            NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius).fill()
            return true
        }
    }
}
