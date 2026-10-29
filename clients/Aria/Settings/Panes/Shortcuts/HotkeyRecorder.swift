import SwiftUI

/// One action's shortcut: its Aria key combo, a recorded chord, or off.
struct HotkeyRecorder: View {
    let title: String
    let hotkey: Aria_Daemon_V1_Hotkey
    /// The combo this action holds unless changed.
    let combo: Aria_Daemon_V1_HotkeyCombo
    let ariaKey: HotkeyKey
    @ObservedObject var binding: ChordBinding
    let set: (HotkeyBinding?) -> Void

    var body: some View {
        ChordRecorder(title: title, chord: hotkey.storedChord, isInUse: binding.wasRefused) {
            set(.chord($0))
        } clear: {
            set(nil)
        } accessory: {
            if case .combo(let held) = hotkey.storedBinding {
                Text(ariaKey.gesture(for: held)).foregroundStyle(.secondary)
                ClearShortcutButton { set(nil) }
            } else {
                Button("Use Aria Key") { set(.combo(combo)) }
            }
        }
    }
}
