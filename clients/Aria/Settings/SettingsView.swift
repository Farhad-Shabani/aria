import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var poller: StatusPoller

    var body: some View {
        TabView {
            GeneralSettings(preferences: preferences)
                .tabItem { Label("General", systemImage: "gearshape") }
            ShortcutsSettings(preferences: preferences)
                .tabItem { Label("Shortcuts", systemImage: "keyboard") }
            DictationSettings(preferences: preferences)
                .tabItem { Label("Dictation", systemImage: "mic") }
            // Phone pairing ships only in the official build, which sets this flag.
            #if PHONE_PAIRING
                PhoneSettings(poller: poller)
                    .tabItem { Label("Phone", systemImage: "iphone") }
            #endif
            AboutSettings()
                .tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 520)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear {
            NSApplication.shared.activate()
            Task { await preferences.sync() }
        }
    }
}

struct GeneralSettings: View {
    @ObservedObject var preferences: DaemonPreferences
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.enabledByDefault

    var body: some View {
        Form {
            LabeledContent("Startup:") {
                Toggle("Launch at login", isOn: $launchAtLogin)
            }
            LabeledContent("Overlay:") {
                Toggle("Show status pill", isOn: $statusPillEnabled)
            }
            .settingsGroup()
            LabeledContent("History:") {
                Toggle(
                    "Save dictation history",
                    isOn: Binding(
                        get: { preferences.historyEnabled },
                        set: { on in Task { await preferences.set { $0.dictationHistory = on } } }
                    )
                )
                .disabled(preferences.snapshot == nil)
                .help("Text is kept on this Mac for up to 30 days.")
            }
            .settingsGroup()
        }
        .formStyle(.columns)
        .padding(20)
        .onAppear { launchAtLogin = SMAppService.mainApp.status == .enabled }
        .onChange(of: launchAtLogin) { _, enabled in
            // The resets above and in the catch would otherwise re-enter here.
            guard enabled != (SMAppService.mainApp.status == .enabled) else { return }
            do {
                if enabled {
                    try SMAppService.mainApp.register()
                } else {
                    try SMAppService.mainApp.unregister()
                }
            } catch {
                NSLog("Launch at Login toggle failed: \(error)")
                launchAtLogin = SMAppService.mainApp.status == .enabled
            }
        }
    }
}

struct ShortcutsSettings: View {
    @ObservedObject var preferences: DaemonPreferences

    var body: some View {
        Form {
            LabeledContent("Dictation hotkey:") {
                Picker(
                    "Dictation hotkey",
                    selection: Binding(
                        get: { preferences.hotkey ?? .rightOption },
                        set: { key in Task { await preferences.set { $0.dictationHotkey = key.wire } } }
                    )
                ) {
                    ForEach(HotkeyKey.allCases, id: \.self) { key in
                        Text(key.title).tag(key)
                    }
                }
                .labelsHidden()
                .fixedSize()
                .disabled(preferences.hotkey == nil)
            }
            AskHotkeyRecorder(preferences: preferences)
                .settingsGroup()
        }
        .formStyle(.columns)
        .padding(20)
    }
}

struct AboutSettings: View {
    private let info = Bundle.main.infoDictionary ?? [:]

    var body: some View {
        VStack(spacing: 8) {
            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable()
                .frame(width: 64, height: 64)
            Text(info["CFBundleDisplayName"] as? String ?? "Aria")
                .font(.title2)
            Text(
                "Version \(info["CFBundleShortVersionString"] as? String ?? "?") "
                    + "(\(info["CFBundleVersion"] as? String ?? "?"))"
            )
            .foregroundStyle(.secondary)
        }
        .padding(24)
    }
}

extension View {
    /// Space between one labelled group of controls and the one before.
    func settingsGroup() -> some View {
        padding(.top, 12)
    }
}

extension Text {
    /// Explanatory text under a checkbox, indented past its box.
    func settingsFootnote() -> some View {
        font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.leading, 20)
    }
}
