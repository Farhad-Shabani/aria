/// The Mac's hotkeys, read off the shared snapshot. Only the Mac knows the
/// key types, so these stay out of the shared file.
extension Aria_Daemon_V1_UserPreferences {
    /// Nil when unset or when the key is one this build does not know.
    var storedDictationHotkey: HotkeyKey? {
        HotkeyKey(wire: dictationHotkey)
    }
}
