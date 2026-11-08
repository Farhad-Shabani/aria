import SwiftUI

enum SelectionFill {
    static let cornerRadius: CGFloat = 8
}

extension View {
    /// The fill that marks the selected row or block, as a source list's does.
    func selectionFill(_ isSelected: Bool) -> some View {
        background(
            isSelected ? AnyShapeStyle(.quaternary) : AnyShapeStyle(.clear),
            in: RoundedRectangle(cornerRadius: SelectionFill.cornerRadius))
    }
}
