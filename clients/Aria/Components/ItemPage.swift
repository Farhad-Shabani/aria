import SwiftUI

private enum Layout {
    static let spacing: CGFloat = 10
}

/// One item's page: a header naming it, then the item's own sections.
struct ItemPage<Icon: View, Accessory: View, Content: View>: View {
    let title: String
    var subtitle: String?
    @ViewBuilder let icon: Icon
    @ViewBuilder let accessory: Accessory
    @ViewBuilder let content: Content

    var body: some View {
        Section {
            HStack(spacing: Layout.spacing) {
                icon
                HeadlineStack(title: title) {
                    if let subtitle {
                        Text(subtitle)
                    }
                }
                Spacer()
                accessory
            }
        }
        content
    }
}

extension ItemPage where Accessory == EmptyView {
    init(
        title: String,
        subtitle: String? = nil,
        @ViewBuilder icon: () -> Icon,
        @ViewBuilder content: () -> Content
    ) {
        self.init(title: title, subtitle: subtitle, icon: icon, accessory: { EmptyView() }, content: content)
    }
}
