import SwiftUI

struct PrivacySettings: View {
    let settings: DaemonSettings
    @State private var confirmingMemory = false

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section("History") {
                Toggle(isOn: editor.binding(\.historyEnabled, \.historyEnabled)) {
                    Text(SettingsRow.saveHistory.title)
                    Text(
                        "Dictations, transcripts, and chats stay on your Mac for up to 30 days."
                    )
                }
                .settingsRow(.saveHistory)
                Toggle(isOn: editor.binding(\.historyKeepAudio, \.historyKeepAudio)) {
                    Text(SettingsRow.keepAudio.title)
                    Text(
                        "Lets you transcribe a dictation again. Audio stays on your Mac and may capture people nearby.")
                }
                .disabled(!editor.snapshot.historyEnabled)
                .settingsRow(.keepAudio)
            }
            Section {
                Toggle(
                    SettingsRow.memory.title,
                    isOn: Self.memorySwitch(
                        editor.binding(\.memoryEnabled, \.memoryEnabled),
                        confirm: { confirmingMemory = true }
                    )
                )
                .settingsRow(.memory)
                .confirmationDialog("Turn On Memory?", isPresented: $confirmingMemory) {
                    Button("Turn On") { editor.set(\.memoryEnabled, to: true) }
                } message: {
                    Text(
                        "Aria sends the requests you speak to Claude to find facts about you. This includes Local Provider chats."
                    )
                }
            } footer: {
                Text("Aria sends the requests you speak to Claude to remember facts.")
            }
        }
    }

    /// Turning Memory on sends text off the Mac, so it calls `confirm` instead of writing.
    static func memorySwitch(_ stored: Binding<Bool>, confirm: @escaping () -> Void) -> Binding<Bool> {
        Binding(
            get: { stored.wrappedValue },
            set: { isOn in
                if isOn { confirm() } else { stored.wrappedValue = false }
            }
        )
    }
}
