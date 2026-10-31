import Combine

/// A page to show when the Settings window may not exist yet.
@MainActor
final class SettingsPaneRequest: ObservableObject {
    @Published var pane: SettingsPane?
}
