import SwiftUI

struct SettingsView: View {
    private static let paneKey = "settingsPane"

    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var poller: StatusPoller
    let updates: UpdateController
    @State private var history = SettingsHistory(
        current: UserDefaults.standard.string(forKey: paneKey).flatMap(SettingsPane.init) ?? .general
    )

    var body: some View {
        SidebarWindow {
            SettingsSidebar(
                selection: Binding(get: { history.current }, set: { history.open($0) }),
                daemon: poller.state
            )
        } detail: {
            page
                .toolbar {
                    ToolbarItemGroup(placement: .navigation) {
                        Button("Back", systemImage: "chevron.left") { history.goBack() }
                            .disabled(!history.canGoBack)
                            .keyboardShortcut("[", modifiers: .command)
                        Button("Forward", systemImage: "chevron.right") { history.goForward() }
                            .disabled(!history.canGoForward)
                            .keyboardShortcut("]", modifiers: .command)
                    }
                }
        }
        .onChange(of: history.current) { _, pane in
            UserDefaults.standard.set(pane.rawValue, forKey: Self.paneKey)
        }
        .onAppear { Task { await preferences.sync() } }
    }

    @ViewBuilder
    private var page: some View {
        switch history.current {
        case .general: GeneralSettings(updates: updates)
        case .dictation: DictationSettings(preferences: preferences)
        case .recognition: RecognitionSettings(preferences: preferences)
        case .shortcuts: ShortcutsSettings(preferences: preferences)
        case .privacy: PrivacySettings(preferences: preferences)
        case .phone: PhoneSettings(poller: poller)
        case .about: AboutSettings()
        }
    }
}
