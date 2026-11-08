import SwiftUI

struct BarDivider: View {
    private static let height: CGFloat = 14

    var body: some View {
        Divider().frame(height: Self.height)
    }
}
