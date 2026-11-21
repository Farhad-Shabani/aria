import SwiftUI

struct OpenAriaPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    /// Nil when the shortcut is off.
    let shortcut: ShortcutBinding?

    var body: some View {
        Form {
            Section {
                switch shortcut {
                case .combo(let combo):
                    VStack(spacing: 8) {
                        AriaKeySpot(key: ariaKey, combo: combo)
                        Text(ariaKey.gesture(for: combo))
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .accessibilityHidden(true)
                    }
                    .padding(.vertical, 8)
                    if !state.isHotkeyReady {
                        AriaKeyAccessNote(state: state.ariaKeyState)
                    }
                case .chord(let chord):
                    ChordLabel(chord: chord)
                        .font(.title)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 8)
                case nil:
                    Text("The shortcut is off. Turn it on in Settings > Shortcuts.")
                        .foregroundStyle(.secondary)
                }
            } footer: {
                Text("Search commands, prompts, and history.")
            }
        }
    }
}
