import SwiftUI

/// Sizes setup's pages share, so a change to one reaches them all.
enum SetupMetrics {
    /// Room above and below a drawing in its section.
    static let pictureInset: CGFloat = 8
    /// Between a drawing or hero and the words under it.
    static let captionSpacing: CGFloat = 8
    /// Between the lines of a section footer.
    static let footerSpacing: CGFloat = 4
    /// How strongly a drawing's picked-out part takes the tint.
    static let highlightOpacity = 0.15
    static let highlightStroke: CGFloat = 1.5
}

extension View {
    /// A drawing centered in its section, clear of the rows around it.
    func setupPicture() -> some View {
        frame(maxWidth: .infinity).padding(.vertical, SetupMetrics.pictureInset)
    }
}
