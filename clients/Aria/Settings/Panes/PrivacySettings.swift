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
                    Text("Keep recordings for retry")
                    Text("Lets you re-transcribe any dictation for 24 hours. Recordings may include other voices.")
                }
                .disabled(!editor.snapshot.dictationHistory)
            }
        }
    }
}
