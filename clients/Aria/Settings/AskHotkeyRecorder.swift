import Carbon.HIToolbox
import SwiftUI

/// Turns the ask chord on and off, and captures a new one.
///
/// A local monitor is enough to capture: it needs no Accessibility grant and
/// only sees keys while this window is key, which is exactly the capture
/// window.
struct AskHotkeyRecorder: View {
    @ObservedObject var preferences: DaemonPreferences
    @State private var capturing = false
    @State private var monitor: Any?
    @State private var refusal: String?

    var body: some View {
        LabeledContent("Ask hotkey:") {
            Toggle(
                "Enabled",
                isOn: Binding(
                    get: { preferences.askHotkey?.enabled ?? false },
                    set: { on in turn(on: on) }
                )
            )
            .disabled(preferences.askHotkey == nil)
        }
        LabeledContent("Chord:") {
            VStack(alignment: .leading, spacing: 4) {
                Button(title) {
                    if capturing {
                        endCapture()
                    } else {
                        beginCapture()
                    }
                }
                .disabled(preferences.askHotkey?.enabled != true)
                if let refusal {
                    Text(refusal)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
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
        return preferences.askHotkey?.title ?? "—"
    }

    private func turn(on: Bool) {
        guard let hotkey = preferences.askHotkey else { return }
        endCapture()
        Task { await preferences.set { $0.askHotkey = hotkey.with(enabled: on).wire } }
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
            Task { await preferences.set { $0.askHotkey = hotkey.wire } }
        }
    }
}

extension AskHotkey {
    /// The chord as a menu would write it, modifiers in Apple's order.
    var title: String {
        var glyphs = ""
        if control { glyphs += "⌃" }
        if option { glyphs += "⌥" }
        if shift { glyphs += "⇧" }
        if command { glyphs += "⌘" }
        return glyphs + keyName
    }

    /// What the key prints, asked of the layout in use so it reads correctly
    /// on a keyboard that is not US QWERTY. Keys that print nothing, or
    /// print whitespace, are named instead.
    private var keyName: String {
        if let named = Self.namedKeys[keyCode] { return named }
        guard let printed = printedKey, !printed.isEmpty else { return "Key \(keyCode)" }
        return printed
    }

    private var printedKey: String? {
        guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
            let property = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData)
        else { return nil }
        let data = Unmanaged<CFData>.fromOpaque(property).takeUnretainedValue()
        guard let bytes = CFDataGetBytePtr(data) else { return nil }
        return bytes.withMemoryRebound(to: UCKeyboardLayout.self, capacity: 1) { layout in
            var deadKeys: UInt32 = 0
            var length = 0
            var characters = [UniChar](repeating: 0, count: 4)
            let status = UCKeyTranslate(
                layout,
                keyCode,
                UInt16(kUCKeyActionDisplay),
                0,
                UInt32(LMGetKbdType()),
                OptionBits(kUCKeyTranslateNoDeadKeysBit),
                &deadKeys,
                characters.count,
                &length,
                &characters
            )
            guard status == noErr, length > 0 else { return nil }
            // Left as the key prints it. An uppercase letter beside the
            // glyphs reads as if Shift were part of the chord.
            return String(utf16CodeUnits: characters, count: length)
        }
    }

    private static let namedKeys: [UInt16: String] = [
        UInt16(kVK_Space): "Space",
        UInt16(kVK_Return): "Return",
        UInt16(kVK_Tab): "Tab",
        UInt16(kVK_Delete): "Delete",
        UInt16(kVK_ForwardDelete): "Forward Delete",
        UInt16(kVK_Escape): "Escape",
        UInt16(kVK_LeftArrow): "←",
        UInt16(kVK_RightArrow): "→",
        UInt16(kVK_UpArrow): "↑",
        UInt16(kVK_DownArrow): "↓",
        UInt16(kVK_Home): "Home",
        UInt16(kVK_End): "End",
        UInt16(kVK_PageUp): "Page Up",
        UInt16(kVK_PageDown): "Page Down",
        UInt16(kVK_F1): "F1",
        UInt16(kVK_F2): "F2",
        UInt16(kVK_F3): "F3",
        UInt16(kVK_F4): "F4",
        UInt16(kVK_F5): "F5",
        UInt16(kVK_F6): "F6",
        UInt16(kVK_F7): "F7",
        UInt16(kVK_F8): "F8",
        UInt16(kVK_F9): "F9",
        UInt16(kVK_F10): "F10",
        UInt16(kVK_F11): "F11",
        UInt16(kVK_F12): "F12",
    ]
}
