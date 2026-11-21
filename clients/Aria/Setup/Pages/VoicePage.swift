import SwiftUI

struct VoicePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                TextField("Name", text: $state.voiceName, prompt: Text("As transcripts should show it"))
                    .disabled(state.isBusy)
            } footer: {
                if state.voiceSampling == nil {
                    Text(
                        "You’ll read \(VoiceProfiles.samples) short sentences aloud. macOS may ask for the microphone.")
                }
                NoticeText(notice: state.notice)
            }
            if let sampling = state.voiceSampling {
                Section {
                    SentenceReading(sampling: sampling)
                        .padding(.vertical, 8)
                }
            }
        }
    }
}
