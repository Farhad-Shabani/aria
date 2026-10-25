/// A dictation setting an app can choose for itself.
enum AppSetting: CaseIterable {
    case output
    case delivery

    /// The group's settings, plus any the app already sets, so a stored
    /// value never applies unseen.
    static func shown(for bundleID: String, overrides: Aria_Daemon_V1_DictationOverrides) -> [AppSetting] {
        let offered = AppGroup.of(bundleID: bundleID)?.settings ?? allCases
        return allCases.filter { offered.contains($0) || $0.isSet(in: overrides) }
    }

    var title: String {
        switch self {
        case .output: "Output"
        case .delivery: "Deliver"
        }
    }

    func isSet(in overrides: Aria_Daemon_V1_DictationOverrides) -> Bool {
        switch self {
        case .output: overrides.hasOutputMode
        case .delivery: overrides.hasDeliveryMode
        }
    }
}
