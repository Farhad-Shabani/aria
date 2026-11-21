import SwiftUI

/// The last page: the shortcut that opens Aria, then the other two gestures
/// to keep. Pressing the shortcut ends setup in Aria itself.
struct OpenAriaPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    /// Nil when the shortcut is off.
    let shortcut: ShortcutBinding?
    /// Nil when the Ask shortcut holds a chord or nothing.
    let askCombo: ShortcutCombo?

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
            Section {
                LabeledContent("Dictate", value: state.isHotkeyReady ? ariaKey.title : "Needs permission")
                if state.isHotkeyReady, let askCombo {
                    LabeledContent("Ask your assistant", value: ariaKey.gesture(for: askCombo))
                }
            } footer: {
                NoticeText(notice: state.notice)
            }
        }
    }
}
