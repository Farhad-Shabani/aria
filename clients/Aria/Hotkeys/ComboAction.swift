/// What an Aria key combo runs. The daemon reports only the combo; the
/// stored shortcuts say which action, if any, holds it.
enum ComboAction {
    case ask
    case promptList

    init?(combo: Aria_Daemon_V1_ShortcutCombo, in settings: Aria_Daemon_V1_Settings) {
        if settings.askShortcut.storedBinding == .combo(combo) {
            self = .ask
        } else if settings.promptListShortcut.storedBinding == .combo(combo) {
            self = .promptList
        } else {
            return nil
        }
    }
}
