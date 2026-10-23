import SwiftUI

private enum Layout {
    static let spacing: CGFloat = 10
    static let verticalPadding: CGFloat = 4
}

struct SidebarHeader<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: Layout.spacing) {
            AppIcon(side: SettingsMetrics.headerIconSide)
            content
        }
        .padding(.vertical, Layout.verticalPadding)
    }
}
