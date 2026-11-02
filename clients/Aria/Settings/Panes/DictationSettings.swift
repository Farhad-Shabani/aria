import SwiftUI

struct DictationSettings: View {
    let settings: DaemonSettings

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section("Text") {
                output(editor)
                delivery(editor)
            }
            Section {
                Toggle("Spoken corrections", isOn: editor.binding(\.backtrack, \.backtrack))
                    .disabled(
                        !DictationEdits.allowsSpokenCorrections(DictationEdits.effectiveDelivery(in: editor.snapshot)))
            } footer: {
                Text("Applies corrections you say as you dictate. Needs **Deliver** set to After recording stops.")
            }
            Section("Recording") {
                Toggle(isOn: editor.binding(\.muteSound, \.muteSound)) {
                    Text("Mute while recording")
                    Text("System audio comes back when recording stops.")
                }
                autoStop(editor)
            }
        }
    }

    private func output(_ editor: SettingsEditor) -> some View {
        Picker(selection: editor.binding(\.outputMode, \.outputMode)) {
            ForEach(DictationEdits.outputModes, id: \.self) { mode in
                Text(mode.title).tag(mode)
            }
        } label: {
            Text("Output")
            if DictationEdits.locksDelivery(editor.snapshot.outputMode) {
                Text("Copying always delivers after recording stops.")
            }
        }
    }

    private func delivery(_ editor: SettingsEditor) -> some View {
        Picker(
            "Deliver",
            selection: editor.binding(DictationEdits.effectiveDelivery(in: editor.snapshot)) { mode, edit in
                edit.deliveryMode = mode
            }
        ) {
            ForEach(DictationEdits.deliveryModes, id: \.self) { mode in
                Text(mode.title).tag(mode)
            }
        }
        .disabled(DictationEdits.locksDelivery(editor.snapshot.outputMode))
    }

    private func autoStop(_ editor: SettingsEditor) -> some View {
        Picker(selection: editor.binding(\.autoStopMs, \.autoStopMs)) {
            ForEach(DictationEdits.autoStopChoices(including: editor.snapshot.autoStopMs), id: \.self) {
                Text(DictationEdits.autoStopTitle($0)).tag($0)
            }
        } label: {
            Text("Auto-stop")
            Text("Stops a tapped recording after this much silence.")
        }
    }
}
