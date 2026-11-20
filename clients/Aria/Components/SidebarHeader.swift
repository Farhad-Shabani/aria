import SwiftUI

enum HeaderMetrics {
    /// The gap between a header icon and its title.
    static let iconSpacing: CGFloat = 10
}

private enum Layout {
    static let verticalPadding: CGFloat = 8
}

struct SidebarHeader<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: HeaderMetrics.iconSpacing) {
            // The app's card atop a sidebar, 44 pt like the Apple Account card. The app
            // icon has no 44 pt size, and scaling another blurs the mark's hairlines.
            Image(.cardIcon)
                .accessibilityHidden(true)
            content
        }
        .padding(.vertical, Layout.verticalPadding)
    }
}
