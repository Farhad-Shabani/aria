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
                    setup: setup, engineChange: engineChange
                )
                .settingsRow(.speechModel)
                deliveryPicker(editor, shown: delivery, unavailable: unavailable)
                    .settingsRow(.deliver)
            }
            Section {
                output(editor)
                    .settingsRow(.output)
                Toggle(isOn: editor.binding(\.cleanupEnabled, \.cleanupEnabled)) {
                    Text(SettingsRow.cleanUpText.title)
                    Text("Removes filler words like “um” and repeated words.")
                }
                .settingsRow(.cleanUpText)
            } header: {
                Text("Text")
            }
            Section("Recording") {
                MicrophonePicker(editor: editor)
                    .settingsRow(.microphone)
                Toggle(isOn: editor.binding(\.muteSound, \.muteSound)) {
                    Text(SettingsRow.muteWhileRecording.title)
                    Text("Mutes other apps. Their sound comes back when recording stops.")
                }
                .settingsRow(.muteWhileRecording)
                Toggle(isOn: editor.binding(\.returnSends, \.returnSends)) {
                    Text(SettingsRow.returnSends.title)
                    Text("Pressing Return stops recording, then sends what you said.")
                }
                .settingsRow(.returnSends)
                autoStop(editor)
                    .settingsRow(.autoStop)
            }
        }
    }

    private func output(_ editor: SettingsEditor) -> some View {
        Picker(SettingsRow.output.title, selection: editor.binding(\.outputMode, \.outputMode)) {
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
            Text(SettingsRow.deliver.title)
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
            Text(SettingsRow.autoStop.title)
            Text("Stops a tapped recording after this much silence.")
        }
    }
}
