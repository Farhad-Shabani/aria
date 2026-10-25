import SwiftUI

/// The one recorder capturing, if any: a second one ends it first, so a
/// chord is never saved to two settings.
@MainActor
private enum ActiveCapture {
    static var end: (() -> Void)?
}

/// Captures with a local monitor, which needs no Accessibility grant. It sees
/// keys only while the Settings window is key, and that is the only time
/// capture runs.
struct ChordRecorder: View {
    let title: String
    let chord: Chord?
    /// Carbon turned the chord down, so another app holds it.
    let isInUse: Bool
    let record: (Chord) -> Void
    /// Removes the chord, from the ⓧ beside it or Delete while recording.
    let clear: () -> Void
    @Environment(\.isEnabled) private var isEnabled
    @State private var monitor: Any?
    @State private var refusal: String?

    var body: some View {
        LabeledContent {
            HStack {
                if chord != nil && !isCapturing {
                    Button("Clear Shortcut", systemImage: "xmark.circle.fill", action: clear)
                        .labelStyle(.iconOnly)
                        .buttonStyle(.borderless)
                        .foregroundStyle(.secondary)
                        .help("Clear Shortcut")
                }
                Button {
                    if isCapturing {
                        endCapture()
                    } else {
                        beginCapture()
                    }
                } label: {
                    if !isCapturing, let chord {
                        ChordLabel(chord: chord)
                    } else {
                        Text(isCapturing ? "Press keys…" : "Record")
                    }
                }
            }
        } label: {
            // One branch per line count: an empty `if` can still leave room
            // for a second line and lift the row off center.
            if let warning {
                Text(title)
                Text(warning).foregroundStyle(.red)
            } else {
                Text(title)
            }
        }
        // The row can go away, lose its window's focus, or be disabled
        // mid-capture. A monitor left behind would keep swallowing keys.
        .onDisappear(perform: endCapture)
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didResignKeyNotification)) { _ in
            endCapture()
        }
        .onChange(of: isEnabled) { _, enabled in
            if !enabled { endCapture() }
        }
    }

    private var isCapturing: Bool { monitor != nil }

    private var warning: String? {
        refusal ?? (isInUse ? "Shortcut is in use" : nil)
    }

    private func beginCapture() {
        ActiveCapture.end?()
        ActiveCapture.end = endCapture
        refusal = nil
        GlobalShortcut.pause(true)
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            capture(event)
            // Swallowed, so the chord being recorded never reaches the app.
            return nil
        }
    }

    private func endCapture() {
        refusal = nil
        guard let monitor else { return }
        NSEvent.removeMonitor(monitor)
        self.monitor = nil
        ActiveCapture.end = nil
        GlobalShortcut.pause(false)
    }

    private func capture(_ event: NSEvent) {
        switch Chord.captured(keyCode: event.keyCode, modifiers: event.modifierFlags) {
        case .cancelled:
            endCapture()
        case .cleared:
            endCapture()
            clear()
        case .refused(let reason):
            refusal = reason
        case .chord(let chord):
            endCapture()
            record(chord)
        }
    }
}
