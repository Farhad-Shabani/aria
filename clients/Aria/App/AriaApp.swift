import SwiftUI

@main
struct AriaApp: App {
    private static let userGuide = "https://ariacue.pages.dev/user-guide/"

    @StateObject private var poller: StatusPoller
    @StateObject private var phonePairing: PhonePairing
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var settings = DaemonSettings(transport: SettingsClient())

    init() {
        let poller = StatusPoller(client: DaemonProcess())
        _poller = StateObject(wrappedValue: poller)
        _phonePairing = StateObject(wrappedValue: PhonePairing(poller: poller))
    }

    var body: some Scene {
        MenuBarExtra {
            VStack(spacing: 8) {
                HStack(spacing: 16) {
                    DaemonStatusLabel(state: poller.state)
                    Spacer(minLength: 16)
                    Toggle(
                        "Aria",
                        isOn: Binding(
                            get: { poller.state == .running },
                            set: { _ in Task { await poller.toggle() } }
                        )
                    )
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .disabled(poller.state == .working)
                }
                .padding(8)
                SystemTranscriptionStatusRow(status: appDelegate.systemTranscriptionStatus)
                if let message = poller.failure?.message {
                    Text(markdown: message)
                        .font(.caption)
                        .foregroundStyle(.failure)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 8)
                }
                FinishSetupMenuItem(
                    setup: appDelegate.setupStatus, health: appDelegate.settingsHealth, poller: poller
                ) {
                    appDelegate.settingsPaneRequest.pane = .finishSetup
                    appDelegate.settings.request()
                }
                Divider()
                VStack(spacing: 2) {
                    Button {
                        appDelegate.settings.request()
                    } label: {
                        HStack {
                            Text("Settings…")
                            Spacer()
                            Text("⌘,").foregroundStyle(.secondary)
                        }
                    }
                    .keyboardShortcut(",")
                    CheckForUpdatesButton(updates: appDelegate.updates)
                }
                Divider()
                Button {
                    NSApplication.shared.terminate(nil)
                } label: {
                    HStack {
                        Text("Quit Aria")
                        Spacer()
                        Text("⌘Q").foregroundStyle(.secondary)
                    }
                }
                .keyboardShortcut("q")
            }
            .buttonStyle(MenuActionStyle())
            .padding(8)
            .frame(width: 280)
            .onAppear {
                let setup = appDelegate.setupStatus
                Task { await setup.refresh() }
            }
        } label: {
            MenuBarLabel(
                poller: poller,
                phonePairing: phonePairing,
                status: appDelegate.status,
                systemTranscriptionStatus: appDelegate.systemTranscriptionStatus,
                updates: appDelegate.updates,
                settings: settings,
                controller: appDelegate
            )
        }
        .menuBarExtraStyle(.window)
        .commands {
            CommandGroup(replacing: .appInfo) {
                Button("About Aria") {
                    appDelegate.settingsPaneRequest.pane = .about
                    appDelegate.settings.request()
                }
                CheckForUpdatesCommand(updates: appDelegate.updates)
            }
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { appDelegate.settings.request() }
                    .keyboardShortcut(",")
            }
            CommandGroup(replacing: .newItem) {
                Button("Open Settings File") {
                    let health = appDelegate.settingsHealth
                    let settings = appDelegate.settings
                    Task {
                        await health.openSettingsFile()
                        // A failed open reports inside Settings, so bring it up.
                        if health.openFailure != nil { settings.request() }
                    }
                }
            }
            CommandGroup(replacing: .sidebar) {}
            CommandGroup(replacing: .toolbar) {}
            CommandGroup(replacing: .help) {
                Button("Aria User Guide") {
                    if let url = URL(string: Self.userGuide) {
                        NSWorkspace.shared.open(url)
                    }
                }
            }
        }

        // A Window, not a Settings scene: only a Window takes the unified
        // toolbar that puts Back and Forward beside the sidebar.
        Window("Aria Settings", id: WindowID.settings) {
            SettingsView(
                settings: settings,
                poller: poller,
                phonePairing: phonePairing,
                updates: appDelegate.updates,
                promptHotkeys: appDelegate.promptHotkeys,
                askChord: appDelegate.askShortcut.chord,
                paletteChord: appDelegate.palette.chord,
                setupStatus: appDelegate.setupStatus,
                modelDownload: appDelegate.modelDownload,
                sttEngineChange: appDelegate.sttEngineChange,
                setupState: appDelegate.setupState,
                voices: appDelegate.voices,
                serverStt: appDelegate.serverStt,
                settingsHealth: appDelegate.settingsHealth,
                paneRequest: appDelegate.settingsPaneRequest
            )
            .environmentObject(appDelegate.setupRunner)
        }
        .sidebarWindowScene()
        .commandsRemoved()

        Window("Aria Setup", id: WindowID.setup) {
            SetupView(state: appDelegate.setupState, settings: settings)
                .environmentObject(appDelegate.setupRunner)
        }
        .sidebarWindowScene()
        .commandsRemoved()
    }
}

private struct MenuBarLabel: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var poller: StatusPoller
    let phonePairing: PhonePairing
    @ObservedObject var status: StatusController
    @ObservedObject var systemTranscriptionStatus: SystemTranscriptionStatus
    @ObservedObject var updates: UpdateController
    let settings: DaemonSettings
    let controller: AppDelegate

    var body: some View {
        let state = MenuBarState(
            daemon: poller.state, pill: status.live, isSystemTranscriptionRunning: systemTranscriptionStatus.isRunning,
            isUpdateAvailable: updates.isUpdateAvailable)
        Image(nsImage: MenuBarMark.image(state))
            .accessibilityLabel(state.accessibilityLabel)
            .onAppear {
                let openWindow = openWindow
                controller.settings.register {
                    openWindow(id: WindowID.settings)
                    NSApplication.shared.activate()
                }
                controller.setup.register {
                    openWindow(id: WindowID.setup)
                    NSApplication.shared.activate()
                }
                poller.startPolling()
                controller.trackHotkeys(settings)
                controller.palette.track(poller, phonePairing: phonePairing)
                Task { await settings.sync() }
                let health = controller.settingsHealth
                Task { await health.refresh() }
            }
            // A start that failed may have found something left to set up.
            .onChange(of: poller.failure) { _, _ in
                let setup = controller.setupStatus
                Task { await setup.refresh() }
            }
            // A daemon that came up after launch has settings nobody read.
            .onChange(of: poller.state) { _, state in
                guard state == .running else { return }
                Task { await settings.sync() }
                let health = controller.settingsHealth
                Task { await health.refresh() }
            }
    }
}

private struct FinishSetupMenuItem: View {
    @ObservedObject var setup: SetupStatus
    @ObservedObject var health: SettingsHealth
    @ObservedObject var poller: StatusPoller
    let open: () -> Void

    var body: some View {
        let items = SetupItem.all(setup: setup, health: health, poller: poller)
        if !items.isEmpty {
            Button(action: open) {
                HStack {
                    Text("Finish Setting Up Aria…")
                    Spacer()
                    CountBadge(count: items.count)
                }
            }
        }
    }
}

private struct CheckForUpdatesButton: View {
    private static let dotSide: CGFloat = 6
    @ObservedObject var updates: UpdateController

    var body: some View {
        Button {
            updates.checkForUpdates()
        } label: {
            HStack {
                Text(updates.actionTitle)
                Spacer()
                if updates.isUpdateAvailable {
                    Circle().fill(.tint).frame(width: Self.dotSide, height: Self.dotSide)
                        .accessibilityHidden(true)
                }
            }
        }
        .disabled(!updates.canCheck)
    }
}

private struct CheckForUpdatesCommand: View {
    @ObservedObject var updates: UpdateController

    var body: some View {
        Button(updates.actionTitle) { updates.checkForUpdates() }
            .disabled(!updates.canCheck)
    }
}

private struct MenuActionStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled
    @State private var isHovered = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(isEnabled ? .primary : .tertiary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .contentShape(Rectangle())
            .background {
                RoundedRectangle(cornerRadius: 5)
                    .fill(.primary.opacity(configuration.isPressed ? 0.12 : isHovered && isEnabled ? 0.06 : 0))
            }
            .onHover { isHovered = $0 }
    }
}
