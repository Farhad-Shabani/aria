/// The Aria key: a bare modifier the daemon reads. It dictates, ends a run
/// a press started, and makes a combo with Shift or Command.
enum AriaKey: CaseIterable {
    case rightOption
    case fn
    case off

    /// The key the daemon uses when none is stored.
    static let `default` = AriaKey.rightOption

    var title: String {
        switch self {
        case .rightOption: return "Right Option"
        case .fn: return "Fn"
        case .off: return "Off"
        }
    }

    func gesture(for combo: Aria_Daemon_V1_ShortcutCombo) -> String {
        self == .off ? title : "\(title) + \(combo.symbol)"
    }

    /// With the key off, nothing is owed: an idle listener isn't a failure.
    static func isReady(key: AriaKey?, isServing: Bool) -> Bool {
        key == .off || isServing
    }

    var wire: Aria_Daemon_V1_AriaKey {
        switch self {
        case .rightOption: return .rightOption
        case .fn: return .fn
        case .off: return .off
        }
    }

    /// Nil for `unspecified` and values this build does not know.
    init?(wire: Aria_Daemon_V1_AriaKey) {
        switch wire {
        case .rightOption: self = .rightOption
        case .fn: self = .fn
        case .off: self = .off
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }
}

/// Read off the shared snapshot here: only the Mac knows the key type, so
/// this stays out of the shared settings code.
extension Aria_Daemon_V1_Settings {
    /// Nil when unset or when the key is one this build does not know.
    var storedAriaKey: AriaKey? {
        AriaKey(wire: ariaKey)
    }
}
