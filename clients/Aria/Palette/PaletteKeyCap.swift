import SwiftUI

struct PaletteKeyCap: View {
    private static let minSide: CGFloat = 18
    private static let cornerRadius: CGFloat = 4

    private let key: String

    var body: some View {
        Text(key)
            .font(.caption.weight(.semibold))
            .monospacedDigit()
            .foregroundStyle(.secondary)
            // A two-glyph key such as ⌘↩ would otherwise wrap in a tight footer.
            .fixedSize()
            .padding(.horizontal, 4)
            .frame(minWidth: Self.minSide, minHeight: Self.minSide)
            .background(.quinary, in: RoundedRectangle(cornerRadius: Self.cornerRadius))
    }
}

extension PaletteKeyCap {
    init(_ key: String) {
        self.key = key
    }
}
