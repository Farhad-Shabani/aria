import SwiftUI

extension Scene {
    /// The unified toolbar is what lets `SidebarWindow`'s sidebar reach the
    /// top of the window.
    func sidebarWindowScene() -> some Scene {
        defaultLaunchBehavior(.suppressed)
            .restorationBehavior(.disabled)
            .windowResizability(.contentSize)
            .windowToolbarStyle(.unified)
    }
}
