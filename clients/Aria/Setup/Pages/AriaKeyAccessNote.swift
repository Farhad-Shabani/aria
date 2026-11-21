import SwiftUI

/// Why the Aria key doesn't answer yet, and where to turn it on.
struct AriaKeyAccessNote: View {
    let state: AriaKeyState

    var body: some View {
        Group {
            if state == .refused {
                Text(markdown: "macOS didn’t let Aria hear the Aria key. Run `aria doctor` in Terminal to see why.")
            } else {
                Text(
                    "To use the Aria key, turn on Aria in System Settings > Privacy & Security > \(state.privacyPane)."
                )
            }
        }
        .foregroundStyle(.secondary)
    }
}
