import SwiftUI

struct VoicePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                TextField("Name", text: $state.voiceName, prompt: Text("As transcripts should show it"))
                if let prompt = state.voicePrompt {
                    Text(prompt).fontWeight(.medium)
                }
            } footer: {
                Text("Say a few sentences when asked. macOS may ask for the microphone.")
                NoticeText(notice: state.notice)
            }
        }
    }
}
