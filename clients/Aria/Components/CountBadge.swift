import SwiftUI

/// A red count of what's left to set up, as System Settings shows in its sidebar.
struct CountBadge: View {
    let count: Int
    @Environment(\.controlSize) private var controlSize

    private var isSmall: Bool { controlSize == .small || controlSize == .mini }

    var body: some View {
        Text(count, format: .number)
            .font((isSmall ? Font.caption2 : .caption).weight(.semibold))
            .monospacedDigit()
            .foregroundStyle(.white)
            .padding(.horizontal, isSmall ? 4 : 6)
            .padding(.vertical, isSmall ? 0 : 1)
            .background(.red, in: Capsule())
            .accessibilityLabel(count == 1 ? "1 step" : "\(count) steps")
    }
}
