import SwiftUI

struct GeneralSettings: View {
    @Environment(\.openWindow) private var openWindow
    @StateObject private var loginItem = LoginItem()
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.enabledByDefault
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.enabledByDefault
    @AppStorage(AppDelegate.captionsEnabledKey)
    private var captionsEnabled = AppDelegate.captionsEnabledByDefault

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
                Toggle(isOn: $captionsEnabled) {
                    Text("Show captions")
                    Text("What Aria hears appears in a panel while you speak.")
                }
            }
            Section {
                LabeledContent {
                    Button("Set Up Aria…") { openWindow(id: WindowID.setup) }
                } label: {
                    Text("Setup")
                    Text("Engines, hotkey access, and your voice.")
                }
            }
        }
        .onAppear { loginItem.refresh() }
    }
}
