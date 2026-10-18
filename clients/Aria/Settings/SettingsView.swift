import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var poller: StatusPoller
    let updates: UpdateController

    var body: some View {
        TabView {
            GeneralSettings(preferences: preferences)
                .tabItem { Label("General", systemImage: "gearshape") }
            ShortcutsSettings(preferences: preferences)
                .tabItem { Label("Shortcuts", systemImage: "keyboard") }
            DictationSettings(preferences: preferences)
                .tabItem { Label("Dictation", systemImage: "mic") }
            PhoneSettings(poller: poller)
                .tabItem { Label("Phone", systemImage: "iphone") }
            AboutSettings(updates: updates)
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
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.enabledByDefault

    var body: some View {
        Form {
            LabeledContent("Startup:") {
                Toggle("Launch at login", isOn: $launchAtLogin)
            }
            LabeledContent("On screen:") {
                VStack(alignment: .leading) {
                    Toggle("Show status pill", isOn: $statusPillEnabled)
                    Toggle("Show inline preview", isOn: $ghostTextEnabled)
                        .help("Words Aria hasn't typed yet appear faintly at the cursor.")
                }
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
    @ObservedObject var updates: UpdateController
    private let info = Bundle.main.infoDictionary ?? [:]
    private static let legal = "https://github.com/Farhad-Shabani/aria/blob/main/legal/"
    private let licenses = Bundle.main.resourceURL?.appending(path: "Licenses")

    var body: some View {
        VStack(spacing: 16) {
            HStack(spacing: 16) {
                Image(nsImage: NSApplication.shared.applicationIconImage)
                    .resizable()
                    .frame(width: 64, height: 64)
                VStack(alignment: .leading, spacing: 2) {
                    Text(info["CFBundleDisplayName"] as? String ?? "Aria")
                        .font(.title2)
                    Text("Version \(info["CFBundleShortVersionString"] as? String ?? "?")")
                        .foregroundStyle(.secondary)
                    if let copyright = info["NSHumanReadableCopyright"] as? String {
                        Text(copyright)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Divider()
            VStack(alignment: .leading, spacing: 6) {
                Toggle(
                    "Automatically check for updates",
                    isOn: Binding(get: { updates.checksAutomatically }, set: { updates.setChecksAutomatically($0) })
                )
                Toggle(
                    "Automatically download and install",
                    isOn: Binding(get: { updates.installsAutomatically }, set: { updates.setInstallsAutomatically($0) })
                )
                .disabled(!updates.checksAutomatically)
            }
            Button("Check for Updates…") { updates.checkForUpdates() }
                .disabled(!updates.canCheck)
            Divider()
            HStack(spacing: 16) {
                Button("Privacy Policy") { open(Self.legal + "privacy-policy.md") }
                Button("Terms of Service") { open(Self.legal + "terms.md") }
                if let licenses, FileManager.default.fileExists(atPath: licenses.path) {
                    Button("Licenses") { NSWorkspace.shared.open(licenses) }
                }
            }
            .buttonStyle(.link)
        }
        .padding(24)
    }

    private func open(_ link: String) {
        if let url = URL(string: link) { NSWorkspace.shared.open(url) }
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
