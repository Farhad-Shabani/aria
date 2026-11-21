extension AriaKeyState {
    /// The Privacy & Security pane that gives Aria this permission, as the
    /// running macOS names it.
    var privacyPane: String {
        if self == .needsInputMonitoring {
            return "Input Monitoring"
        }
        if #available(macOS 27, *) {
            return "Device Control and Data Access"
        }
        return "Accessibility"
    }
}
