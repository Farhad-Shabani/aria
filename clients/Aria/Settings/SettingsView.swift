import SwiftUI

struct SettingsView: View {
    private static let paneKey = "settingsPane"

    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var poller: StatusPoller
    let updates: UpdateController
    let promptHotkeys: PromptHotkeys
    let askChord: ChordBinding
    let promptListChord: ChordBinding
    @ObservedObject var setupStatus: SetupStatus
    @State private var history = SettingsHistory(
        current: .pane(UserDefaults.standard.string(forKey: paneKey).flatMap(SettingsPane.init) ?? .general)
    )

    var body: some View {
        SidebarWindow {
            SettingsSidebar(
                selection: Binding(get: { history.current.pane }, set: { history.open(.pane($0)) }),
                daemon: poller.state,
                setup: setupStatus
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
        .onChange(of: history.current.pane) { _, pane in
            UserDefaults.standard.set(pane.rawValue, forKey: Self.paneKey)
        }
        .onAppear { Task { await preferences.sync() } }
        // Returning from System Settings or closing the setup window both
        // make this window key, and either may have finished a step.
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { _ in
            Task { await setupStatus.refresh() }
        }
    }

    @ViewBuilder
    private var page: some View {
        switch history.current {
        case .pane(let pane): page(pane)
        case .item(let pane, let id): item(id, in: pane)
        }
    }

    @ViewBuilder
    private func item(_ id: String, in pane: SettingsPane) -> some View {
        let snapshot = preferences.snapshot
        switch pane {
        case .apps
        where snapshot.flatMap({ ApplicationOverrideEdits.overrides(for: id, in: $0.applicationOverrides) }) != nil:
            ApplicationSettings(bundleID: id, preferences: preferences)
                .id(id)
        case .prompts where snapshot.flatMap({ PromptEdits.entry(id, in: $0.prompts) }) != nil:
            PromptSettings(id: id, preferences: preferences, hotkeys: promptHotkeys)
                .id(id)
        default:
            // A pane with no item pages, or an item removed since it opened.
            page(pane)
        }
    }

    @ViewBuilder
    private func page(_ pane: SettingsPane) -> some View {
        let openPrompt = { history.open(.item(.prompts, id: $0)) }
        switch pane {
        case .general: GeneralSettings(setup: setupStatus)
        case .dictation: DictationSettings(preferences: preferences)
        case .recognition: RecognitionSettings(preferences: preferences)
        case .shortcuts:
            ShortcutsSettings(
                preferences: preferences,
                askChord: askChord,
                promptListChord: promptListChord,
                openPrompt: openPrompt
            )
        case .prompts:
            PromptsSettings(preferences: preferences, marked: history.lastItem(in: .prompts), open: openPrompt)
        case .apps:
            AppsSettings(preferences: preferences, marked: history.lastItem(in: .apps)) {
                history.open(.item(.apps, id: $0))
            }
        case .privacy: PrivacySettings(preferences: preferences)
        case .phone: PhoneSettings(poller: poller)
        case .about: AboutSettings(updates: updates)
        }
    }
}
