import SwiftUI

/// A red count of what's left to set up, as System Settings shows in its sidebar.
struct CountBadge: View {
    let count: Int

    var body: some View {
        Text(count, format: .number)
            .font(.caption.weight(.semibold))
            .monospacedDigit()
            .foregroundStyle(.white)
            .padding(.horizontal, 6)
            .padding(.vertical, 1)
            .background(.red, in: Capsule())
            .accessibilityLabel(count == 1 ? "1 step" : "\(count) steps")
    }
}
