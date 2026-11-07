import SwiftUI

struct PaletteRow<Content: View, Accessory: View>: View {
    let symbol: String
    let isSelected: Bool
    /// How far a row nested under another sits in, such as a prompt result.
    var indent: CGFloat = 0
    var isSymbolAnimating = false
    /// `nil` while the row holds a field: a button's label can't take focus.
    let action: (() -> Void)?
    @ViewBuilder let content: Content
    @ViewBuilder let accessory: Accessory
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        // Side by side, since a button's label can't hold another button.
        HStack(spacing: 10) {
            Group {
                if let action {
                    Button(action: action) { label }
                        .buttonStyle(.plain)
                } else {
                    label
                }
            }
            .accessibilityAddTraits(isSelected ? .isSelected : [])
            accessory
        }
        .padding(.leading, PaletteRowMetrics.padding + indent)
        .padding(.trailing, PaletteRowMetrics.padding)
        .modifier(PaletteRowHighlight(isSelected: isSelected))
    }

    private var label: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .symbolEffect(
                    .variableColor.iterative, options: .repeating, isActive: isSymbolAnimating && !reduceMotion
                )
                .foregroundStyle(.secondary)
                .frame(width: PaletteRowMetrics.symbolWidth)
                .accessibilityHidden(true)
            content
        }
        .frame(maxHeight: .infinity)
        .contentShape(Rectangle())
    }
}

extension PaletteRow where Accessory == EmptyView {
    init(
        symbol: String, isSelected: Bool, indent: CGFloat = 0, action: (() -> Void)?,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            symbol: symbol, isSelected: isSelected, indent: indent, action: action, content: content,
            accessory: { EmptyView() })
    }
}

extension View {
    /// A row's main text: one line, cut short well before the label beside it.
    func paletteRowText(truncation: Text.TruncationMode = .tail) -> some View {
        lineLimit(1)
            .truncationMode(truncation)
            .padding(.trailing, 24)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    func paletteRowDetail() -> some View {
        font(.caption)
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .fixedSize()
    }
}

enum PaletteRowMetrics {
    static let symbolWidth: CGFloat = 20
    /// Between the highlight's edges and what the row holds.
    static let padding: CGFloat = 10
}

/// A palette row's height, and the fill that marks it selected.
struct PaletteRowHighlight: ViewModifier {
    private static let height: CGFloat = 36

    let isSelected: Bool

    func body(content: Content) -> some View {
        content
            .frame(minHeight: Self.height)
            .selectionFill(isSelected)
    }
}
