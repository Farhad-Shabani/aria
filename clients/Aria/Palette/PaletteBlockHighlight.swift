import SwiftUI

/// The fill that marks the current block of an entry's page, as a list row's
/// does. Every block keeps the inset, so text doesn't shift as focus moves.
struct PaletteBlockHighlight: ViewModifier {
    static let inset: CGFloat = 8

    let isFocused: Bool

    func body(content: Content) -> some View {
        content
            .padding(Self.inset)
            .selectionFill(isFocused)
            .accessibilityAddTraits(isFocused ? .isSelected : [])
    }
}
