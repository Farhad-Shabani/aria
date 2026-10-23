import SwiftUI

struct DaemonStatusLabel: View {
    private static let dotSide: CGFloat = 7

    let state: DaemonState

    var body: some View {
        HeadlineStack(title: Bundle.main.displayName) {
            HStack(spacing: 5) {
                Circle()
                    .fill(state.statusColor)
                    .frame(width: Self.dotSide, height: Self.dotSide)
                Text(state.statusLine)
            }
        }
        .accessibilityElement(children: .combine)
    }
}
