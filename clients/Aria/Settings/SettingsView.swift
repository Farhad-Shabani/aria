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
        // Always both columns, as System Settings shows them.
        NavigationSplitView(columnVisibility: .constant(.all)) {
            SettingsSidebar(
                selection: Binding(get: { history.current }, set: { history.open($0) }),
                daemon: poller.state
            )
            .frame(minWidth: SettingsMetrics.sidebarWidth)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            page
                .toolbar(removing: .title)
                .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
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
        .frame(width: SettingsMetrics.windowWidth)
        .frame(minHeight: SettingsMetrics.minimumHeight)
        .onChange(of: history.current) { _, pane in
            UserDefaults.standard.set(pane.rawValue, forKey: Self.paneKey)
        }
        .onAppear {
            NSApplication.shared.activate()
            Task { await preferences.sync() }
        }
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
