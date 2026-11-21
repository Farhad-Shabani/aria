import SwiftUI

struct AriaKeyAccessNote: View {
    let keyState: AriaKeyState

    var body: some View {
        Group {
            if keyState == .refused {
                Text(markdown: "macOS didn’t let Aria hear the Aria key. Run `aria doctor` in Terminal to see why.")
            } else {
                Text(
                    "To use the Aria key, turn on Aria in System Settings > Privacy & Security > \(keyState.privacyPane)."
                )
            }
        }
        .foregroundStyle(.secondary)
    }
}
