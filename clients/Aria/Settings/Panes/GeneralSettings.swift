import SwiftUI

struct GeneralSettings: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var setup: SetupStatus
    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var catalog: AssistantCatalog
    let openAssistant: () -> Void
    @StateObject private var loginItem = LoginItem()
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.isEnabledByDefault
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.isEnabledByDefault
    @AppStorage(AppDelegate.captionsEnabledKey)
    private var captionsEnabled = AppDelegate.areCaptionsEnabledByDefault

    var body: some View {
        Form {
            Section("Startup") {
                Toggle(
                    "Launch at login",
                    isOn: Binding(get: { loginItem.isEnabled }, set: { loginItem.set(enabled: $0) })
                )
            }
            Section {
                ItemLink(open: openAssistant) {
                    Text("Assistant")
                    Spacer()
                    Text(assistantName).foregroundStyle(.secondary)
                }
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
                    if setup.required.isEmpty {
                        Button("Open…") { openWindow(id: WindowID.setup) }
                    } else {
                        Button("Finish Setup…") { openWindow(id: WindowID.setup) }
                            .buttonStyle(.borderedProminent)
                    }
                } label: {
                    // One branch per line count, so the row stays centered.
                    if let reason = setup.reason {
                        Text("Aria Setup")
                        Text(reason)
                    } else {
                        Text("Aria Setup")
                    }
                }
            }
        }
        .onAppear { loginItem.refresh() }
    }

    private var assistantName: String {
        guard let snapshot = preferences.snapshot else {
            return preferences.loadFailure == nil ? "Connecting…" : "Unavailable"
        }
        if let assistant = catalog.choices.first(where: { $0.id == snapshot.assistantID }) {
            return assistant.name
        }
        if catalog.isLoading { return "Loading…" }
        if catalog.failure != nil { return "Unavailable" }
        return "Choose…"
    }
}
