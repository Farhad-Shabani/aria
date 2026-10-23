import SwiftUI

struct DaemonStatusLabel: View {
    private static let dotSide: CGFloat = 7

    let state: DaemonState

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(Bundle.main.displayName).font(.headline)
            HStack(spacing: 5) {
                Circle()
                    .fill(state.statusColor)
                    .frame(width: Self.dotSide, height: Self.dotSide)
                Text(state.statusLine)
            }
            .font(.subheadline)
            .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}
