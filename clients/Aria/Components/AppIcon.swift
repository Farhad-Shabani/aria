import SwiftUI

struct AppIcon: View {
    let side: CGFloat

    var body: some View {
        Image(nsImage: NSApplication.shared.applicationIconImage)
            .resizable()
            .frame(width: side, height: side)
            .accessibilityHidden(true)
    }
}
