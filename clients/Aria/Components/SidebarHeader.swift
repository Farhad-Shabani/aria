import SwiftUI

private enum Layout {
    static let verticalPadding: CGFloat = 8
}

struct SidebarHeader<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: HeaderMetrics.iconSpacing) {
            AppIcon(side: HeaderMetrics.cardIconSide)
            content
        }
        .padding(.vertical, Layout.verticalPadding)
    }
}
