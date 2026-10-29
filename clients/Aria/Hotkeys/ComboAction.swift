/// What each Aria key combo runs. The daemon reports only the combo, so this
/// is the one place that pairs a modifier with an action.
enum ComboAction {
    case ask
    case promptList

    /// Nil for a combo this build does not know.
    init?(combo: Aria_Daemon_V1_HotkeyCombo) {
        switch combo {
        case .shift: self = .ask
        case .command: self = .promptList
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    /// The modifier held with the Aria key, as a menu writes it.
    var modifier: String {
        switch self {
        case .ask: return "⇧"
        case .promptList: return "⌘"
        }
    }
}
