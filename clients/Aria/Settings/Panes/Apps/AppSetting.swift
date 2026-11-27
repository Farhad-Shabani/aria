/// A dictation setting an app can choose for itself.
enum AppSetting: CaseIterable {
    case output
    case delivery

    var title: String {
        switch self {
        case .output: "Output"
        case .delivery: "Deliver"
        }
    }
}
