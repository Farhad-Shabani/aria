import CoreGraphics

/// Sizes every Settings view shares: one icon scale for rows, headers, and
/// the About hero, and one layout for sheets.
enum SettingsMetrics {
    static let iconSide: CGFloat = 20
    static let headerIconSide: CGFloat = 32
    static let heroIconSide: CGFloat = 64
    /// The gap between a header icon and its title.
    static let headerSpacing: CGFloat = 10
    static let sheetPadding: CGFloat = 24
    static let sheetSpacing: CGFloat = 14
    static let sheetGlyphSide: CGFloat = 56

    static let glyphRatio: CGFloat = 0.7
}
