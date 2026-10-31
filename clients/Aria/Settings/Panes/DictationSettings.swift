import SwiftUI

struct DictationSettings: View {
    private static let silenceStep = 0.1

    let settings: DaemonSettings
    /// Held locally while the thumb moves, then pushed once when it lands.
    @State private var silenceSeconds = DictationEdits.silenceSecondsRange.lowerBound

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section("Text") {
                output(editor)
                delivery(editor)
            }
            Section {
                Toggle("Spoken corrections", isOn: editor.binding(\.backtrack, \.backtrack))
                    .disabled(!DictationEdits.allowsSpokenCorrections(editor.snapshot.deliveryMode))
            } footer: {
                Text("Applies corrections you say as you dictate. Needs **Deliver** set to After recording stops.")
            }
            Section("Recording") {
                autoStop(editor)
                silence(editor)
                Toggle(isOn: editor.binding(\.muteSound, \.muteSound)) {
                    Text("Mute sound while recording")
                    Text("Sound returns when recording stops.")
                }
            }
        }
    }

    private func output(_ editor: SettingsEditor) -> some View {
        Picker(selection: editor.binding(editor.snapshot.outputMode, edit: DictationEdits.setOutput)) {
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
        Picker("Deliver", selection: editor.binding(\.deliveryMode, \.deliveryMode)) {
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

    private func silence(_ editor: SettingsEditor) -> some View {
        LabeledContent {
            HStack {
                // A step would make macOS draw tick marks, so the value is rounded when the thumb lands instead.
                Slider(value: $silenceSeconds, in: DictationEdits.silenceSecondsRange) { editing in
                    guard !editing else { return }
                    silenceSeconds = (silenceSeconds / Self.silenceStep).rounded() * Self.silenceStep
                    editor.set(\.silenceDurationMs, to: DictationEdits.silenceMilliseconds(seconds: silenceSeconds))
                }
                .frame(maxWidth: 160)
                Text("\(silenceSeconds, format: .number.precision(.fractionLength(1))) s of silence")
                    .monospacedDigit()
            }
        } label: {
            Text("Wake-word pause")
            Text("Stops a wake-word recording after this much silence.")
        }
        .onChange(of: editor.snapshot.silenceDurationMs, initial: true) { _, ms in
            silenceSeconds = Double(ms) / 1000
        }
    }
}
