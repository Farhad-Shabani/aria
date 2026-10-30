import SwiftUI

private enum Layout {
    static let verticalPadding: CGFloat = 8
}

struct SidebarHeader<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: SettingsMetrics.headerSpacing) {
            AppIcon(side: SettingsMetrics.cardIconSide)
            content
        }
        .padding(.vertical, Layout.verticalPadding)
    }
}
