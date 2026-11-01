import SwiftUI

enum HeaderMetrics {
    /// The gap between a header icon and its title.
    static let iconSpacing: CGFloat = 10
}

private enum Layout {
    static let verticalPadding: CGFloat = 8
    /// The app's card atop a sidebar, sized like the Apple Account card.
    static let cardIconSide: CGFloat = 44
}

struct SidebarHeader<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: HeaderMetrics.iconSpacing) {
            AppIcon(side: Layout.cardIconSide)
            content
        }
        .padding(.vertical, Layout.verticalPadding)
    }
}
