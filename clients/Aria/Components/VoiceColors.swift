import AppKit
import SwiftUI

/// A color an enrolled voice wears: its ring on the status pill and its
/// monogram in Settings. The six differ only in hue, so none outshines
/// another or the pill's own state. `plain` wears no ring at all.
enum VoiceColor: Int, CaseIterable {
    case blue
    case clay
    case gold
    case olive
    case teal
    case violet
    // Last, so the stored numbers of the colors before it never shift.
    case plain

    /// The colors new voices get; `plain` is only picked by hand.
    static let palette = allCases.filter { $0 != .plain }

    private static let saturation = 0.45
    private static let ringLightness = 0.68
    private static let monogramLightness = 0.55
    private static let swatchSide: CGFloat = 12

    var title: String {
        switch self {
        case .blue: "Blue"
        case .clay: "Clay"
        case .gold: "Gold"
        case .olive: "Olive"
        case .teal: "Teal"
        case .violet: "Violet"
        case .plain: "None"
        }
    }

    var ring: Color? { hue.map { Self.tone(hue: $0, lightness: Self.ringLightness) } }

    /// Darker than the ring, so white initials stay readable.
    var monogram: Color { hue.map { Self.tone(hue: $0, lightness: Self.monogramLightness) } ?? .gray }

    var swatch: NSImage {
        let side = Self.swatchSide
        let image = NSImage(size: NSSize(width: side, height: side), flipped: false) { rect in
            guard let ring else {
                NSColor.secondaryLabelColor.setStroke()
                NSBezierPath(ovalIn: rect.insetBy(dx: 0.5, dy: 0.5)).stroke()
                return true
            }
            NSColor(ring).setFill()
            NSBezierPath(ovalIn: rect).fill()
            return true
        }
        // A menu draws its images as templates unless told otherwise, which
        // would strip the color the item is about.
        image.isTemplate = false
        return image
    }

    private var hue: Double? {
        switch self {
        case .blue: 210
        case .clay: 25
        case .gold: 45
        case .olive: 95
        case .teal: 175
        case .violet: 270
        case .plain: nil
        }
    }

    /// Equal HSL lightness is what makes the six read alike; SwiftUI takes HSB.
    private static func tone(hue: Double, lightness: Double) -> Color {
        let brightness = lightness + saturation * min(lightness, 1 - lightness)
        return Color(hue: hue / 360, saturation: 2 * (1 - lightness / brightness), brightness: brightness)
    }
}

/// Which color each enrolled voice wears. A presentation choice, so the app
/// keeps it; the daemon only names the voice.
@MainActor
final class VoiceColors: ObservableObject {
    private static let key = "voiceColors"

    @Published private(set) var assigned: [String: VoiceColor]
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let stored = defaults.dictionary(forKey: Self.key) as? [String: Int] ?? [:]
        assigned = stored.compactMapValues(VoiceColor.init(rawValue:))
    }

    /// For drawing: never assigns, since a view must not publish changes.
    func color(of name: String) -> VoiceColor? {
        assigned[name]
    }

    /// Gives a voice with no color the next one, and returns its color.
    @discardableResult
    func assignColor(to name: String) -> VoiceColor {
        if let color = assigned[name] { return color }
        let color = Self.next(after: Array(assigned.values))
        assigned[name] = color
        save()
        return color
    }

    /// Matches the enrolled voices: new ones get a color, removed ones free
    /// theirs, however they were removed.
    func sync(enrolled names: [String]) {
        let kept = assigned.filter { names.contains($0.key) }
        if kept.count != assigned.count {
            assigned = kept
            save()
        }
        for name in names { assignColor(to: name) }
    }

    func set(_ color: VoiceColor, for name: String) {
        assigned[name] = color
        save()
    }

    /// The least-used color, ties going to the earliest in the palette, so
    /// voices stay distinct until the palette runs out.
    static func next(after taken: [VoiceColor]) -> VoiceColor {
        VoiceColor.palette.min { left, right in
            let leftUses = taken.filter { $0 == left }.count
            let rightUses = taken.filter { $0 == right }.count
            return (leftUses, left.rawValue) < (rightUses, right.rawValue)
        } ?? .blue
    }

    private func save() {
        defaults.set(assigned.mapValues(\.rawValue), forKey: Self.key)
    }
}
