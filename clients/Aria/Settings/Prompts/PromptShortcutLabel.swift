import SwiftUI

/// A prompt's state as Apple's Settings shows one at a row's end: its
/// shortcut, or On, or Off.
struct PromptShortcutLabel: View {
    let entry: Aria_Daemon_V1_Prompt

    var body: some View {
        Group {
            if !entry.enabled {
                Text("Off")
            } else if let chord = entry.storedChord {
                ChordLabel(chord: chord)
            } else {
                Text("On")
            }
        }
        .foregroundStyle(.secondary)
    }
}
