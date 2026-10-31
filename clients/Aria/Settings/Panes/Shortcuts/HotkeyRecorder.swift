import SwiftUI

/// One action's shortcut: its Aria key combo, a recorded chord, or off.
struct HotkeyRecorder: View {
    let title: String
    let hotkey: Aria_Daemon_V1_Shortcut
    let ariaKey: AriaKey
    @ObservedObject var binding: ChordBinding
    let set: (ShortcutBinding?) -> Void

    var body: some View {
        if case .combo(let held) = hotkey.storedBinding {
            LabeledContent(title) {
                HStack {
                    ClearShortcutButton { set(nil) }
                    Text(ariaKey.gesture(for: held))
                }
            }
        } else {
            ChordRecorder(title: title, chord: hotkey.storedChord, isInUse: binding.wasRefused) {
                set(.chord($0))
            } clear: {
                set(nil)
            }
        }
    }
}
