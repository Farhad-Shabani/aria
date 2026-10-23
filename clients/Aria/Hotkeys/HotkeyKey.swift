/// The bare modifier whose clean tap starts dictation.
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
