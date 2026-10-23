import SwiftUI

struct GeneralSettings: View {
    @ObservedObject var updates: UpdateController
    @StateObject private var loginItem = LoginItem()
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.enabledByDefault
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.enabledByDefault

    var body: some View {
        Form {
            Section("Startup") {
                Toggle(
                    "Launch at login",
                    isOn: Binding(get: { loginItem.isEnabled }, set: { loginItem.set(enabled: $0) })
                )
            }
            Section("On screen") {
                Toggle("Show status pill", isOn: $statusPillEnabled)
                Toggle(isOn: $ghostTextEnabled) {
                    Text("Show inline preview")
                    Text("Words Aria hasn't typed yet appear faintly at the cursor.")
                }
            }
            Section("Updates") {
                Toggle(
                    "Automatically check for updates",
                    isOn: Binding(get: { updates.checksAutomatically }, set: { updates.setChecksAutomatically($0) })
                )
                Toggle(
                    "Automatically download and install",
                    isOn: Binding(get: { updates.installsAutomatically }, set: { updates.setInstallsAutomatically($0) })
                )
                .disabled(!updates.checksAutomatically)
                LabeledContent {
                    Button("Check for Updates…") { updates.checkForUpdates() }
                        .disabled(!updates.canCheck)
                } label: {
                    Text(Bundle.main.displayName)
                    if let version = Bundle.main.shortVersion {
                        Text("Version \(version)")
                    }
                }
            }
        }
        .onAppear { loginItem.refresh() }
    }
}
