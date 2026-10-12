/// The Mac's hotkeys, read off the shared snapshot. Only the Mac knows the
/// key types, so these stay out of the shared file.
extension DaemonPreferences {
    /// Nil until the daemon answers, or for a key this build does not know.
    var hotkey: HotkeyKey? {
        snapshot.flatMap { HotkeyKey(wire: $0.dictationHotkey) }
    }

    /// Nil on the same terms as `hotkey`.
    var askHotkey: AskHotkey? {
        snapshot?.storedAskHotkey
    }
}

extension Aria_Daemon_V1_UserPreferences {
    /// Nil when the daemon predates the ask chord or stored a key code no
    /// keyboard produces.
    var storedAskHotkey: AskHotkey? {
        hasAskHotkey ? AskHotkey(wire: askHotkey) : nil
    }
}
