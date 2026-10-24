import SwiftUI

struct DictationSettings: View {
    private static let silenceStep = 0.1

    let preferences: DaemonPreferences
    /// Held locally while the thumb moves, then pushed once when it lands.
    @State private var silenceSeconds = DictationEdits.silenceSecondsRange.lowerBound

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            Section("Text") {
                output(editor)
                delivery(editor)
            }
            Section("Recording") {
                Toggle("Mute output while recording", isOn: editor.binding(\.muteOutput, \.muteOutput))
                silence(editor)
            }
        }
    }

    private func output(_ editor: PreferencesEditor) -> some View {
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

    private func delivery(_ editor: PreferencesEditor) -> some View {
        Picker("Deliver", selection: editor.binding(\.deliveryMode, \.deliveryMode)) {
            ForEach(DictationEdits.deliveryModes, id: \.self) { mode in
                Text(mode.title).tag(mode)
            }
        }
        .disabled(DictationEdits.locksDelivery(editor.snapshot.outputMode))
    }

    private func silence(_ editor: PreferencesEditor) -> some View {
        LabeledContent {
            HStack {
                // A step would make macOS draw tick marks, so the value is rounded when the thumb lands instead.
                Slider(value: $silenceSeconds, in: DictationEdits.silenceSecondsRange) { editing in
                    guard !editing else { return }
                    silenceSeconds = (silenceSeconds / Self.silenceStep).rounded() * Self.silenceStep
                    editor.set(\.silenceDurationMs, to: DictationEdits.silenceMilliseconds(seconds: silenceSeconds))
                }
                .frame(maxWidth: 160)
                Text(String(format: "%.1f s of silence", silenceSeconds))
                    .monospacedDigit()
            }
        } label: {
            Text("Silence")
            Text("Ends wake-word and command-line dictation without --toggle. Hotkeys stop on tap or release.")
        }
        .onChange(of: editor.snapshot.silenceDurationMs, initial: true) { _, ms in
            silenceSeconds = Double(ms) / 1000
        }
    }
}
