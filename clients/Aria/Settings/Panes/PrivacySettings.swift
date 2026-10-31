import SwiftUI

struct PrivacySettings: View {
    let settings: DaemonSettings

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section("History") {
                Toggle(isOn: editor.binding(\.historyEnabled, \.historyEnabled)) {
                    Text("Save dictation history")
                    Text("Text is kept on your Mac for up to 30 days.")
                }
                Toggle(isOn: editor.binding(\.historyKeepAudio, \.historyKeepAudio)) {
                    Text("Keep audio for 24 hours")
                    Text(
                        "Lets you transcribe a dictation again. Audio stays on your Mac and may capture people nearby.")
                }
                .disabled(!editor.snapshot.historyEnabled)
            }
            Section {
                Toggle("Memory", isOn: editor.binding(\.memoryEnabled, \.memoryEnabled))
            } footer: {
                Text("Asks you speak are sent to Claude to remember facts.")
            }
        }
    }
}
