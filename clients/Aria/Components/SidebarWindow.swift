import SwiftUI

private enum Layout {
    static let width: CGFloat = 700
    static let minimumHeight: CGFloat = 440
    static let sidebarWidth: CGFloat = 200
}

/// Its scene needs `sidebarWindowScene()`, or the sidebar stops short of
/// the top of the window.
struct SidebarWindow<Sidebar: View, Detail: View>: View {
    @ViewBuilder let sidebar: Sidebar
    @ViewBuilder let detail: Detail

    var body: some View {
        // Always both columns, as System Settings shows them.
        NavigationSplitView(columnVisibility: .constant(.all)) {
            sidebar
                .frame(minWidth: Layout.sidebarWidth)
                .toolbar(removing: .sidebarToggle)
        } detail: {
            detail
                .formStyle(.grouped)
                .toolbar(removing: .title)
                .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        }
        .frame(width: Layout.width)
        .frame(minHeight: Layout.minimumHeight)
        // A menu bar app is not active, so its window would open behind others.
        .onAppear { NSApplication.shared.activate() }
    }
}
