import SwiftUI

enum PaletteBarMetrics {
    /// The list's padding, where a row's highlight starts.
    static let listInset: CGFloat = 8
    /// Where a row's symbol column starts. The bars line up with it.
    static let leading = listInset + PaletteRowMetrics.padding
    static let headerHeight: CGFloat = 50
    static let pillsHeight: CGFloat = 34
    static let footerHeight: CGFloat = 38
    /// Where a footer's last key ends. The actions menu lines up with it.
    static let trailing: CGFloat = 10
}

struct PaletteHeader<Title: View, Detail: View>: View {
    let back: () -> Void
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail

    var body: some View {
        HStack(spacing: 10) {
            PaletteBackButton(action: back)
            title
                .font(.headline)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            detail
                .font(.callout)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        // Back's chevron centers on the rows' symbol column.
        .padding(.leading, PaletteBarMetrics.leading + (PaletteRowMetrics.symbolWidth - PaletteBackButton.side) / 2)
        .padding(.trailing, PaletteBarMetrics.leading)
        .frame(minHeight: PaletteBarMetrics.headerHeight)
    }
}

extension PaletteHeader where Detail == EmptyView {
    init(back: @escaping () -> Void, @ViewBuilder title: () -> Title) {
        self.init(back: back, title: title, detail: { EmptyView() })
    }
}

/// A page's top bar and footer. On macOS 26 they float over the scrolling
/// content. It softens as it passes under the top bar, as Settings does, and
/// fades out before the footer, so nothing rules the footer off from the page.
struct PaletteBars<Top: View, Content: View, Bottom: View>: View {
    @ViewBuilder let top: Top
    @ViewBuilder let content: Content
    @ViewBuilder let bottom: Bottom
    /// The bottom bar's height: the footer, plus a follow-up row on some pages.
    @State private var bottomHeight = PaletteBarMetrics.footerHeight

    var body: some View {
        if #available(macOS 26, *) {
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .mask { BottomFade(height: bottomHeight) }
                .safeAreaBar(edge: .top, spacing: 0) { top }
                // An inset, not a bar: a bar draws an edge along its top.
                .safeAreaInset(edge: .bottom, spacing: 0) {
                    bottom.onGeometryChange(for: CGFloat.self) {
                        $0.size.height
                    } action: {
                        bottomHeight = $0
                    }
                }
                .scrollEdgeEffectStyle(.soft, for: .top)
                .scrollEdgeEffectHidden(true, for: .bottom)
        } else {
            VStack(spacing: 0) {
                top
                content.frame(maxWidth: .infinity, maxHeight: .infinity)
                bottom
            }
        }
    }
}

/// Clear across the whole bottom bar, so a row scrolling under it is gone
/// before it reaches the footer's text, and the bar keeps its own room.
private struct BottomFade: View {
    /// The list's own bottom padding, so a list at rest loses no row to it.
    private static let reach = PaletteBarMetrics.listInset
    let height: CGFloat

    var body: some View {
        VStack(spacing: 0) {
            Color.black
            LinearGradient(colors: [.black, .clear], startPoint: .top, endPoint: .bottom)
                .frame(height: Self.reach)
            Color.clear.frame(height: height)
        }
        .ignoresSafeArea()
    }
}
