import SwiftUI

@main
struct AriaApp: App {
    @Environment(\.openWindow) private var openWindow
    @StateObject private var poller = StatusPoller(
        client: DaemonClient()
    )
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var preferences = DaemonPreferences(
        transport: UnixSocketPreferences(),
        captionsDefaultsKey: AppDelegate.enabledKey
    )

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
                if let error = poller.lastError {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 8)
                }
                Divider()
                Toggle(
                    "Show Captions",
                    isOn: Binding(
                        get: { preferences.captionsEnabled },
                        set: { on in
                            Task { await preferences.set { $0.deliveryMode = on ? .complete : .streaming } }
                        }
                    )
                )
                .disabled(preferences.deliveryLocked)
                .toggleStyle(.checkbox)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                Divider()
                VStack(spacing: 2) {
                    Button {
                        openWindow(id: WindowID.dictationHistory)
                        NSApplication.shared.activate()
                    } label: {
                        Text("Dictation History…")
                            .frame(maxWidth: .infinity, alignment: .leading)
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
                    CheckForUpdatesButton(updates: appDelegate.updates)
                    Button {
                        appDelegate.setup.request()
                    } label: {
                        Text("Set Up Aria…")
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                Divider()
                Button {
                    NSApplication.shared.terminate(nil)
                } label: {
                    Text("Quit Aria")
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .buttonStyle(MenuActionStyle())
            .padding(8)
            .frame(width: 280)
        } label: {
            MenuBarLabel(poller: poller, status: appDelegate.status, preferences: preferences, controller: appDelegate)
        }
        .menuBarExtraStyle(.window)

        // A Window, not a Settings scene: only a Window takes the unified
        // toolbar that puts Back and Forward beside the sidebar.
        Window("Aria Settings", id: WindowID.settings) {
            SettingsView(preferences: preferences, poller: poller, updates: appDelegate.updates)
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
            SetupView(state: appDelegate.setupState, preferences: preferences)
        }
        .sidebarWindowScene()
    }
}

private struct MenuBarLabel: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var poller: StatusPoller
    @ObservedObject var status: StatusController
    let preferences: DaemonPreferences
    let controller: AppDelegate

    var body: some View {
        let state = MenuBarState(daemon: poller.state, pill: status.live)
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
                controller.trackAskHotkey(preferences)
                Task { await preferences.sync() }
            }
            // A daemon that came up after launch has preferences nobody read.
            .onChange(of: poller.state) { _, state in
                guard state == .running else { return }
                Task { await preferences.sync() }
            }
    }
}

private struct CheckForUpdatesButton: View {
    @ObservedObject var updates: UpdateController

    var body: some View {
        Button {
            updates.checkForUpdates()
        } label: {
            Text("Check for Updates…")
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .disabled(!updates.canCheck)
    }
}

private struct MenuActionStyle: ButtonStyle {
    @State private var hovered = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .contentShape(Rectangle())
            .background {
                RoundedRectangle(cornerRadius: 5)
                    .fill(.primary.opacity(configuration.isPressed ? 0.12 : hovered ? 0.06 : 0))
            }
            .onHover { hovered = $0 }
    }
}
