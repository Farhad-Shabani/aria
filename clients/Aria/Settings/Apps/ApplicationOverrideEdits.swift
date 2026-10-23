/// Changes to the per-app list, applied through `PreferencesEditor.editLatest`
/// because the daemon replaces the whole list. An app not in the list is
/// left alone.
enum ApplicationOverrideEdits {
    /// The daemon refuses more.
    static let maximumApps = 64

    static func overrides(
        for bundleID: String,
        in apps: Aria_Daemon_V1_ApplicationOverrides
    ) -> Aria_Daemon_V1_DictationOverrides? {
        apps.entries.first { $0.applicationID == bundleID }?.overrides
    }

    static func isExcluded(_ bundleID: String, in apps: Aria_Daemon_V1_ApplicationOverrides) -> Bool {
        apps.entries.first { $0.applicationID == bundleID }?.excluded ?? false
    }

    /// A choice equal to Dictation's reads as Default.
    static func choice<Mode: Equatable>(_ chosen: Mode?, dictation: Mode) -> Mode? {
        chosen == dictation ? nil : chosen
    }

    static func add(_ bundleID: String, to apps: inout Aria_Daemon_V1_ApplicationOverrides) {
        guard overrides(for: bundleID, in: apps) == nil else { return }
        var entry = Aria_Daemon_V1_ApplicationOverride()
        entry.applicationID = bundleID
        entry.overrides = Aria_Daemon_V1_DictationOverrides()
        apps.entries.append(entry)
    }

    static func remove(_ bundleID: String, from apps: inout Aria_Daemon_V1_ApplicationOverrides) {
        apps.entries.removeAll { $0.applicationID == bundleID }
    }

    static func setExcluded(
        _ excluded: Bool,
        for bundleID: String,
        in apps: inout Aria_Daemon_V1_ApplicationOverrides
    ) {
        update(bundleID, in: &apps) { $0.excluded = excluded }
    }

    /// Nil means Default.
    static func setOutput(
        _ mode: Aria_Daemon_V1_OutputMode?,
        for bundleID: String,
        in apps: inout Aria_Daemon_V1_ApplicationOverrides
    ) {
        update(bundleID, in: &apps) { entry in
            if let mode {
                entry.overrides.outputMode = mode
            } else {
                entry.overrides.clearOutputMode()
            }
        }
    }

    /// Nil means Default.
    static func setDelivery(
        _ mode: Aria_Daemon_V1_DeliveryMode?,
        for bundleID: String,
        in apps: inout Aria_Daemon_V1_ApplicationOverrides
    ) {
        update(bundleID, in: &apps) { entry in
            if let mode {
                entry.overrides.deliveryMode = mode
            } else {
                entry.overrides.clearDeliveryMode()
            }
        }
    }

    private static func update(
        _ bundleID: String,
        in apps: inout Aria_Daemon_V1_ApplicationOverrides,
        change: (inout Aria_Daemon_V1_ApplicationOverride) -> Void
    ) {
        guard let index = apps.entries.firstIndex(where: { $0.applicationID == bundleID }) else { return }
        change(&apps.entries[index])
    }
}
