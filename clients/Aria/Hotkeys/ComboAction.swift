/// What an Aria key combo runs. The daemon reports only the combo; the
/// stored shortcuts say which action, if any, holds it.
enum ComboAction {
    case ask
    case promptList

    init?(combo: Aria_Daemon_V1_HotkeyCombo, in preferences: Aria_Daemon_V1_UserPreferences) {
        if preferences.askHotkey.storedBinding == .combo(combo) {
            self = .ask
        } else if preferences.promptListHotkey.storedBinding == .combo(combo) {
            self = .promptList
        } else {
            return nil
        }
    }
}
