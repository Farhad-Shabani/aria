import SwiftUI

struct DictationSettings: View {
    let settings: DaemonSettings
    @ObservedObject var setup: SetupStatus
    let engineChange: SttEngineChange

    var body: some View {
        SettingsForm(settings: settings) { editor in
            let unavailable = DictationEdits.streamingUnavailable(
                for: editor.snapshot.outputMode, in: settings.streamingUnavailable)
            let delivery = DictationEdits.shownDelivery(editor.snapshot.deliveryMode, canStream: unavailable == nil)
            Section {
                SpeechModelPicker(
                    setup: setup, engineChange: engineChange)
                deliveryPicker(editor, shown: delivery, unavailable: unavailable)
            }
            Section {
                output(editor)
                Toggle(isOn: editor.binding(\.cleanupEnabled, \.cleanupEnabled)) {
                    Text("Clean up text")
                    Text("Removes filler words like “um” and repeated words.")
                }
            } header: {
                Text("Text")
            } footer: {
                Text("With **Deliver** set to After recording stops, also applies corrections like “no, Tuesday”.")
            }
            Section("Recording") {
                Toggle(isOn: editor.binding(\.muteSound, \.muteSound)) {
                    Text("Mute while recording")
                    Text("Mutes other apps. Their sound comes back when recording stops.")
                }
                Toggle(isOn: editor.binding(\.returnSends, \.returnSends)) {
                    Text("Return sends")
                    Text("Return in the app you’re dictating into stops recording, types the rest, then sends.")
                }
                autoStop(editor)
            }
        }
    }

    private func output(_ editor: SettingsEditor) -> some View {
        Picker("Output", selection: editor.binding(\.outputMode, \.outputMode)) {
            ForEach(DictationEdits.outputModes, id: \.self) { mode in
                Text(mode.title).tag(mode)
            }
        }
    }

    private func deliveryPicker(
        _ editor: SettingsEditor, shown: Aria_Daemon_V1_DeliveryMode,
        unavailable: Aria_Daemon_V1_StreamingUnavailable?
    ) -> some View {
        Picker(
            selection: editor.binding(shown) { mode, edit in
                edit.deliveryMode = mode
            }
        ) {
            ForEach(DictationEdits.deliveryModes, id: \.self) { mode in
                Text(mode.title).tag(mode)
            }
        } label: {
            Text("Deliver")
            if let unavailable {
                Text(reason(unavailable))
            }
        }
        .disabled(unavailable != nil)
    }

    private func reason(_ reason: Aria_Daemon_V1_StreamingUnavailable) -> String {
        switch reason {
        case .copy:
            return "Copying always delivers after recording stops."
        case .platform:
            return "Aria pastes here, so it delivers after recording stops."
        case .model:
            let models = setup.speechModels
            return "\(models.current.flatMap(models.model)?.label ?? "This model") can’t type while you speak."
        case .unspecified, .UNRECOGNIZED:
            return "Aria delivers after recording stops."
        }
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
