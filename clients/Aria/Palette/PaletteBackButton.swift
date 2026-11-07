import SwiftUI

struct PaletteBackButton: View {
    static let side: CGFloat = 26
    private static let cornerRadius: CGFloat = 6

    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: "chevron.left")
                .frame(width: Self.side, height: Self.side)
                .background(.quinary, in: RoundedRectangle(cornerRadius: Self.cornerRadius))
        }
        .buttonStyle(.plain)
        .help("Back")
        .accessibilityLabel("Back")
    }
}
