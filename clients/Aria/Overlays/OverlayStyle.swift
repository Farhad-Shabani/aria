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
    /// The waves while a voice's words are typed: brighter than recording's
    /// red, with a halo.
    static let typedVoiceTint = Color(red: 1.0, green: 0.50, blue: 0.54)
    static let typedVoiceHalo = typedVoiceTint.opacity(0.8)
    static let typedVoiceHaloRadius: CGFloat = 3.5
    /// Faint, not gone, while a voice is kept out, so the pill still reads
    /// as listening.
    static let keptOutVoiceOpacity = 0.3
    /// A step below a typed voice, so the change to typed stands out.
    static let unjudgedVoiceOpacity = 0.85

    /// Frosted glass, dark enough that white text and a colored glow read
    /// over any window.
    static let frost = Color(red: 0.055, green: 0.055, blue: 0.067).opacity(0.85)
    /// How much of the blur shows. AppKit fixes the radius, so a lighter blur
    /// mixes in the sharp windows behind.
    static let frostBlur = 0.5
    /// The glow of Aria's own work: Ask's color, used nowhere else.
    static let askGlow = Color(red: 0.75, green: 0.35, blue: 0.95)
    /// How opaque that glow is at rest.
    static let askGlowOpacity = 0.45
    /// A faint top light and lit edge, so the pane looks slightly raised.
    static let frostSheen = Color.white.opacity(0.05)
    static let frostEdge = LinearGradient(
        colors: [.white.opacity(0.2), .white.opacity(0.06)],
        startPoint: .top,
        endPoint: .bottom
    )
    static let frostEdgeWidth: CGFloat = 0.75

    /// Recording, and a press that ended without its result.
    static let alertTint = Color(red: 0.88, green: 0.42, blue: 0.45)
    static let workingTint = Color(red: 0.90, green: 0.75, blue: 0.48)
    static let doneTint = Color(red: 0.48, green: 0.80, blue: 0.59)
    /// Stopped on purpose, not failed.
    static let quietTint = Color(white: 0.72)

    /// Room for the longest failure message; the capsule centres in it.
    static let pillWidth: CGFloat = 480
    static let pillBottomMargin: CGFloat = 40
    static let captionWidth: CGFloat = 720
    /// High enough to clear the status pill, so both can show at once.
    static let captionBottomMargin: CGFloat = 96
}
