import CoreGraphics

/// Where the ghost's window goes and how its lines sit, in AppKit screen
/// coordinates. Accessibility measures from the primary screen's top-left
/// corner, and AppKit from its bottom-left.
struct GhostLayout: Equatable {
    /// The width of a ghost whose field reports no frame.
    static let floatingWidth: CGFloat = 480
    static let maxLines: CGFloat = 6

    var frame: CGRect
    var firstLineIndent: CGFloat
    var lineHeight: CGFloat
    /// Lifts each line so its glyphs centre on the caret like the app's own.
    var baselineOffset: CGFloat

    init(caret: CGRect, field: CGRect?, primaryScreenHeight: CGFloat, ascender: CGFloat, descender: CGFloat) {
        lineHeight = caret.height
        baselineOffset = (caret.height - ascender - abs(descender)) / 2
        let tallest = lineHeight * Self.maxLines
        let left: CGFloat
        let width: CGFloat
        let height: CGFloat
        if let field {
            // Later lines wrap at the field's edges, like the app's own text.
            // ponytail: the field's text insets are unknown, so wrapped lines
            // start at its frame; read per-line left edges if they look off.
            left = field.minX
            width = field.width
            firstLineIndent = max(0, caret.minX - field.minX)
            height = min(max(field.maxY - caret.minY, lineHeight), tallest)
        } else {
            left = caret.minX
            width = Self.floatingWidth
            firstLineIndent = 0
            height = tallest
        }
        frame = CGRect(
            x: left,
            y: primaryScreenHeight - caret.minY - height,
            width: width,
            height: height
        )
    }
}
