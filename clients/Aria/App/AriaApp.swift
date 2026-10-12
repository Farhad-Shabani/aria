import SwiftUI

extension DaemonState {
    /// The menu bar mark's ribbon: waving while listening, flat when stopped.
    var markWave: CGFloat {
        switch self {
        case .running, .working: return 1
        case .stopped: return 0
        }
    }

    var markAlpha: CGFloat {
        switch self {
        case .running, .stopped: return 1
        case .working: return 0.5
        }
    }

    var statusLine: String {
        switch self {
        case .running: return "Running"
        case .stopped: return "Stopped"
        case .working: return "Working…"
        }
    }
}

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
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Aria").font(.headline)
                        Text(poller.state.statusLine)
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
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
                        openWindow(id: "dictation-history")
                        NSApplication.shared.activate()
                    } label: {
                        Text("Dictation History…")
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    // Not a SettingsLink: that shows the window without
                    // activating the app, so an open window stays buried.
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
            MenuBarLabel(poller: poller, preferences: preferences, controller: appDelegate)
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView(preferences: preferences, poller: poller)
        }
        .windowResizability(.contentSize)

        Window("Dictation History", id: "dictation-history") {
            DictationHistoryView()
        }
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)
        .defaultSize(width: 800, height: 520)
        .windowResizability(.contentMinSize)

        Window("Aria Setup", id: "setup") {
            SetupView(state: appDelegate.setupState)
        }
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)
        .windowResizability(.contentSize)
    }
}

private struct MenuBarLabel: View {
    @Environment(\.openSettings) private var openSettings
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var poller: StatusPoller
    let preferences: DaemonPreferences
    let controller: AppDelegate

    var body: some View {
        Image(nsImage: MenuBarMark.image(wave: poller.state.markWave, alpha: poller.state.markAlpha))
            .accessibilityLabel("Aria")
            .onAppear {
                let openSettings = openSettings
                controller.settings.register {
                    openSettings()
                    NSApplication.shared.activate()
                }
                let openWindow = openWindow
                controller.setup.register {
                    openWindow(id: "setup")
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
