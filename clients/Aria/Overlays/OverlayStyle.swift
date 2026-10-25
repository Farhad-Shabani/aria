import SwiftUI

/// How the overlays look, and where they sit at the bottom of the screen.
enum OverlayStyle {
    static let fill = Color.black.opacity(0.75)
    static let font = Font.system(size: 12, weight: .medium)
    static let ringWidth: CGFloat = 1

    /// Room for the longest failure message; the capsule centres in it.
    static let pillWidth: CGFloat = 480
    static let pillBottomMargin: CGFloat = 34
    static let captionWidth: CGFloat = 720
    /// High enough to clear the status pill, so both can show at once.
    static let captionBottomMargin: CGFloat = 90
}
