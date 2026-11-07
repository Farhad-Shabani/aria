import SwiftUI

enum PaletteBarMetrics {
    /// The list's padding, where a row's highlight starts.
    static let listInset: CGFloat = 8
    /// Where a row's symbol column starts. The bars line up with it.
    static let leading = listInset + PaletteRowMetrics.padding
    static let headerHeight: CGFloat = 50
    static let pillsHeight: CGFloat = 34
    static let footerHeight: CGFloat = 38
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

struct PalettePageProgress: View {
    private let title: String

    init(_ title: String) {
        self.title = title
    }

    var body: some View {
        VStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text(title).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// A page's top bar and footer. On macOS 26 they float over the scrolling
/// content, which softens as it passes under them, as Settings does.
struct PaletteBars<Top: View, Content: View, Bottom: View>: View {
    @ViewBuilder let top: Top
    @ViewBuilder let content: Content
    @ViewBuilder let bottom: Bottom

    var body: some View {
        if #available(macOS 26, *) {
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .safeAreaBar(edge: .top, spacing: 0) { top }
                .safeAreaBar(edge: .bottom, spacing: 0) { bottom }
                .scrollEdgeEffectStyle(.soft, for: .vertical)
        } else {
            VStack(spacing: 0) {
                top
                content.frame(maxWidth: .infinity, maxHeight: .infinity)
                bottom
            }
        }
    }
}
