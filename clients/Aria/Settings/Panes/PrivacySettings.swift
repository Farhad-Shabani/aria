import SwiftUI

struct PrivacySettings: View {
    let settings: DaemonSettings
    @State private var confirmingMemory = false

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section("History") {
                Toggle(isOn: editor.binding(\.historyEnabled, \.historyEnabled)) {
                    Text("Save history")
                    Text(
                        "Dictations, transcripts, and chats stay on your Mac for up to 30 days."
                    )
                }
                Toggle(isOn: editor.binding(\.historyKeepAudio, \.historyKeepAudio)) {
                    Text("Keep audio for 24 hours")
                    Text(
                        "Lets you transcribe a dictation again. Audio stays on your Mac and may capture people nearby.")
                }
                .disabled(!editor.snapshot.historyEnabled)
            }
            Section {
                Toggle(
                    "Memory",
                    isOn: Self.memorySwitch(
                        editor.binding(\.memoryEnabled, \.memoryEnabled),
                        confirm: { confirmingMemory = true }
                    )
                )
                .confirmationDialog("Turn On Memory?", isPresented: $confirmingMemory) {
                    Button("Turn On") { editor.set(\.memoryEnabled, to: true) }
                } message: {
                    Text(
                        "Aria sends the requests you speak to Claude to find facts about you. This includes chats with a local model."
                    )
                }
            } footer: {
                Text("Aria sends the requests you speak to Claude to remember facts.")
            }
        }
    }

    /// Turning Memory on sends text off your Mac, so it waits for `confirm`.
    static func memorySwitch(_ stored: Binding<Bool>, confirm: @escaping () -> Void) -> Binding<Bool> {
        Binding(
            get: { stored.wrappedValue },
            set: { isOn in
                if isOn { confirm() } else { stored.wrappedValue = false }
            }
        )
    }
}
