import SwiftUI

@main
struct AriaApp: App {
    @Environment(\.openWindow) private var openWindow
    @StateObject private var poller = StatusPoller(
        client: DaemonClient()
    )
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var settings = DaemonSettings(transport: UnixSocketSettings())

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
                SettingsAttentionView(
                    health: appDelegate.settingsHealth, poller: poller, settings: settings
                )
                .padding(.horizontal, 8)
                if let failure = poller.failure, !failure.isSettingsInvalid {
                    Text(markdown: failure.message)
                        .font(.caption)
                        .foregroundStyle(.failure)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 8)
                }
                Divider()
                VStack(spacing: 2) {
                    Button {
                        openWindow(id: WindowID.dictationHistory)
                        NSApplication.shared.activate()
                    } label: {
                        Text("Dictation History")
                    }
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
                    Button("Open Settings File") {
                        let health = appDelegate.settingsHealth
                        Task { await health.openSettingsFile() }
                    }
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
        } label: {
            MenuBarLabel(
                poller: poller,
                status: appDelegate.status,
                updates: appDelegate.updates,
                settings: settings,
                controller: appDelegate
            )
        }
        .menuBarExtraStyle(.window)

        // A Window, not a Settings scene: only a Window takes the unified
        // toolbar that puts Back and Forward beside the sidebar.
        Window("Aria Settings", id: WindowID.settings) {
            SettingsView(
                settings: settings,
                poller: poller,
                updates: appDelegate.updates,
                promptHotkeys: appDelegate.promptHotkeys,
                askChord: appDelegate.askShortcut.chord,
                promptListChord: appDelegate.promptList.chord,
                setupStatus: appDelegate.setupStatus,
                voices: appDelegate.voices,
                serverTranscription: appDelegate.serverTranscription,
                settingsHealth: appDelegate.settingsHealth
            )
        }
        .sidebarWindowScene()
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { appDelegate.settings.request() }
                    .keyboardShortcut(",")
            }
        }

        Window("Dictation History", id: WindowID.dictationHistory) {
            DictationHistoryView()
        }
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)
        .defaultSize(width: 800, height: 520)
        .windowResizability(.contentMinSize)

        Window("Aria Setup", id: WindowID.setup) {
            SetupView(state: appDelegate.setupState, settings: settings)
        }
        .sidebarWindowScene()
    }
}

private struct MenuBarLabel: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var poller: StatusPoller
    @ObservedObject var status: StatusController
    @ObservedObject var updates: UpdateController
    let settings: DaemonSettings
    let controller: AppDelegate

    var body: some View {
        let state = MenuBarState(
            daemon: poller.state, pill: status.live, isUpdateAvailable: updates.isUpdateAvailable)
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
                Task { await settings.sync() }
                let health = controller.settingsHealth
                Task { await health.refresh() }
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
