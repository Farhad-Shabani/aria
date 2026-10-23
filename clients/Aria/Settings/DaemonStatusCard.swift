import SwiftUI

struct DaemonStatusCard: View {
    let state: DaemonState

    var body: some View {
        HStack(spacing: 10) {
            AppIcon(side: SettingsMetrics.appIconSide)
            VStack(alignment: .leading, spacing: 1) {
                Text(Bundle.main.displayName).font(.headline)
                HStack(spacing: 5) {
                    Circle()
                        .fill(state.statusColor)
                        .frame(width: SettingsMetrics.statusDotSide, height: SettingsMetrics.statusDotSide)
                    Text(state.statusLine)
                }
                .font(.subheadline)
                .foregroundStyle(.secondary)
            }
        }
        .accessibilityElement(children: .combine)
    }
}
