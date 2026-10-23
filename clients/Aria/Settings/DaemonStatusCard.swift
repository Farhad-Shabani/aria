import SwiftUI

struct DaemonStatusCard: View {
    let state: DaemonState

    var body: some View {
        HStack(spacing: 10) {
            AppIcon(side: SettingsMetrics.appIconSide)
            DaemonStatusLabel(state: state)
        }
    }
}
