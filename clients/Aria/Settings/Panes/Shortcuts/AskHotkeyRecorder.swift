import SwiftUI

/// Captures with a local monitor, which needs no Accessibility grant. It sees
/// keys only while the Settings window is key, and that is the only time
/// capture runs.
struct AskHotkeyRecorder: View {
    let editor: PreferencesEditor
    @State private var capturing = false
    @State private var monitor: Any?
    @State private var refusal: String?

    var body: some View {
        Toggle(
            "Ask hotkey",
            isOn: Binding(
                get: { editor.snapshot.storedAskHotkey?.enabled ?? false },
                set: { on in turn(on: on) }
            )
        )
        .disabled(editor.snapshot.storedAskHotkey == nil)
        LabeledContent {
            Button(title) {
                if capturing {
                    endCapture()
                } else {
                    beginCapture()
                }
            }
            .disabled(editor.snapshot.storedAskHotkey?.enabled != true)
        } label: {
            Text("Chord")
            if let refusal {
                Text(refusal)
            }
        }
        // The window can close mid-capture, and a monitor left behind would
        // keep swallowing keys.
        .onDisappear(perform: endCapture)
    }

    private var title: String {
        if capturing {
            return "Press a chord…"
        }
        return editor.snapshot.storedAskHotkey?.title ?? "—"
    }

    private func turn(on: Bool) {
        guard let hotkey = editor.snapshot.storedAskHotkey else { return }
        endCapture()
        editor.set(\.askHotkey, to: hotkey.with(enabled: on).wire)
    }

    private func beginCapture() {
        capturing = true
        refusal = nil
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            record(event)
            // Swallowed, so the chord being recorded never reaches the app.
            return nil
        }
    }

    private func endCapture() {
        capturing = false
        refusal = nil
        if let monitor {
            NSEvent.removeMonitor(monitor)
        }
        monitor = nil
    }

    private func record(_ event: NSEvent) {
        switch AskHotkey.captured(keyCode: event.keyCode, modifiers: event.modifierFlags) {
        case .cancelled:
            endCapture()
        case .refused(let reason):
            refusal = reason
        case .chord(let hotkey):
            endCapture()
            editor.set(\.askHotkey, to: hotkey.wire)
        }
    }
}
