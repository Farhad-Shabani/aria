import SwiftUI

struct SettingsView: View {
    private static let paneKey = "settingsPane"

    @ObservedObject var settings: DaemonSettings
    @ObservedObject var poller: StatusPoller
    let updates: UpdateController
    let promptHotkeys: PromptHotkeys
    let askChord: ChordBinding
    let promptListChord: ChordBinding
    @ObservedObject var setupStatus: SetupStatus
    let voices: VoiceProfiles
    let serverTranscription: ServerTranscription
    let settingsHealth: SettingsHealth
    @ObservedObject var paneRequest: SettingsPaneRequest
    @StateObject private var assistantCatalog = AssistantCatalog()
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
                .safeAreaInset(edge: .top, spacing: 0) {
                    VStack(spacing: 8) {
                        SettingsAttentionView(health: settingsHealth, poller: poller, settings: settings)
                    }
                    .padding(.horizontal, 20)
                    .padding(.vertical, 4)
                }
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
        .onChange(of: paneRequest.pane, initial: true) { _, pane in
            guard let pane else { return }
            history.open(.pane(pane))
            paneRequest.pane = nil
        }
        .task { await settings.sync() }
        .task { await assistantCatalog.load() }
        .onChange(of: poller.state) { _, state in
            guard state == .running else { return }
            Task { await assistantCatalog.loadUnlessReady() }
        }
        // Returning from System Settings or closing the setup window both
        // make this window key, and either may have finished a step.
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { _ in
            Task {
                await settings.sync()
                await setupStatus.refresh()
                await voices.refresh()
                await assistantCatalog.loadUnlessReady()
            }
        }
    }

    @ViewBuilder
    private var page: some View {
        switch history.current {
        case .pane(let pane): page(pane)
        case .assistant: AssistantSettings(settings: settings, catalog: assistantCatalog)
        case .item(let pane, let id): item(id, in: pane)
        }
    }

    @ViewBuilder
    private func item(_ id: String, in pane: SettingsPane) -> some View {
        let snapshot = settings.snapshot
        switch pane {
        case .apps
        where snapshot.flatMap({ ApplicationOverrideEdits.overrides(for: id, in: $0.applicationOverrides) }) != nil:
            ApplicationSettings(bundleID: id, settings: settings)
                .id(id)
        case .prompts where snapshot.flatMap({ PromptEdits.entry(id, in: $0.prompts) }) != nil:
            PromptSettings(id: id, settings: settings, hotkeys: promptHotkeys)
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
        case .general:
            GeneralSettings(setup: setupStatus, settings: settings, catalog: assistantCatalog) {
                history.open(.assistant)
            }
        case .dictation: DictationSettings(settings: settings)
        case .recognition:
            RecognitionSettings(
                settings: settings, voices: voices, serverTranscription: serverTranscription,
                setup: setupStatus)
        case .shortcuts:
            ShortcutsSettings(
                settings: settings,
                askChord: askChord,
                promptListChord: promptListChord,
                openPrompt: openPrompt
            )
        case .prompts:
            PromptsSettings(settings: settings, selected: selection(for: .prompts), open: openPrompt)
        case .apps:
            AppsSettings(settings: settings, selected: selection(for: .apps)) {
                history.open(.item(.apps, id: $0))
            }
        case .privacy: PrivacySettings(settings: settings)
        case .phone: PhoneSettings(poller: poller)
        case .about: AboutSettings(updates: updates)
        }
    }

    private func selection(for pane: SettingsPane) -> Binding<String?> {
        Binding(get: { history.selectedItem(in: pane) }, set: { history.selectItem($0, in: pane) })
    }
}
