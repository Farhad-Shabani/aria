/// The Aria key: a bare modifier that dictates when tapped or held, and runs
/// Ask or the prompt list with Shift or Command. The daemon reads it.
enum HotkeyKey: CaseIterable {
    case rightOption
    case fn

    /// The key the daemon uses when none is stored.
    static let `default` = HotkeyKey.rightOption

    var title: String {
        switch self {
        case .rightOption: return "Right Option"
        case .fn: return "Fn"
        }
    }

    /// Hold the key, tap the action's modifier, let go.
    func gesture(for action: ComboAction) -> String {
        "\(title) + \(action.modifier)"
    }

    var wire: Aria_Daemon_V1_HotkeyKey {
        switch self {
        case .rightOption: return .rightOption
        case .fn: return .fn
        }
    }

    /// Nil for `unspecified` and values this build does not know.
    init?(wire: Aria_Daemon_V1_HotkeyKey) {
        switch wire {
        case .rightOption: self = .rightOption
        case .fn: self = .fn
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }
}

/// Read off the shared snapshot here: only the Mac knows the key type, so
/// this stays out of the shared preferences code.
extension Aria_Daemon_V1_UserPreferences {
    /// Nil when unset or when the key is one this build does not know.
    var storedDictationHotkey: HotkeyKey? {
        HotkeyKey(wire: dictationHotkey)
    }
}
