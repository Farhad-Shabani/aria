import SwiftUI

/// How the overlays look, and where they sit at the bottom of the screen.
enum OverlayStyle {
    static let fill = Color.black.opacity(0.75)
    static let ink = Color.white
    static let dimInk = Color.white.opacity(0.5)
    static let cardCornerRadius: CGFloat = 12
    static let labelFont = Font.system(size: 12, weight: .medium)
    static let captionFont = Font.system(size: 13, weight: .medium)
    static let glyphFont = Font.system(size: 10, weight: .bold)
    static let ringWidth: CGFloat = 1

    /// Recording, and a press that ended without its result.
    static let alertTint = Color(red: 0.88, green: 0.42, blue: 0.45)
    static let workingTint = Color(red: 0.90, green: 0.75, blue: 0.48)
    static let doneTint = Color(red: 0.48, green: 0.80, blue: 0.59)
    /// Stopped on purpose, not failed.
    static let quietTint = Color(white: 0.72)

    /// Room for the longest failure message; the capsule centres in it.
    static let pillWidth: CGFloat = 480
    static let pillBottomMargin: CGFloat = 34
    static let captionWidth: CGFloat = 720
    /// High enough to clear the status pill, so both can show at once.
    static let captionBottomMargin: CGFloat = 90
}
