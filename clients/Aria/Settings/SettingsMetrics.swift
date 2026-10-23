import CoreGraphics

/// One icon scale for every Settings view: rows, headers, and the About hero.
enum SettingsMetrics {
    static let iconSide: CGFloat = 20
    static let headerIconSide: CGFloat = 32
    static let heroIconSide: CGFloat = 64

    /// A tile's corner radius as a share of its side, matching System Settings.
    static let cornerRatio: CGFloat = 0.225
    static let glyphRatio: CGFloat = 0.6
}
