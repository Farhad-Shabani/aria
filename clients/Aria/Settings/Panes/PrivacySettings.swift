import SwiftUI

struct PrivacySettings: View {
    let preferences: DaemonPreferences

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            Section("History") {
                Toggle(isOn: editor.binding(\.dictationHistory, \.dictationHistory)) {
                    Text("Save dictation history")
                    Text("Text is kept on this Mac for up to 30 days.")
                }
                Toggle(isOn: editor.binding(\.historyAudioRetention, \.historyAudioRetention)) {
                    Text("Keep audio for 24 hours")
                    Text(
                        "Lets you transcribe a dictation again. Audio stays on this Mac and may capture people nearby.")
                }
                .disabled(!editor.snapshot.dictationHistory)
            }
        }
    }
}
