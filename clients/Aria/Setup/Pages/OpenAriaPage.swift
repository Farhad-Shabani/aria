import SwiftUI

struct OpenAriaPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    /// Nil when the daemon hasn't said what the shortcut is.
    let shortcut: Aria_Daemon_V1_Shortcut?
    /// Nil when the Ask shortcut holds a chord or nothing.
    let askCombo: ShortcutCombo?

    var body: some View {
        Form {
            Section {
                switch shortcut?.storedBinding {
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
                        AriaKeyAccessNote(keyState: state.ariaKeyState)
                    }
                case .chord(let chord):
                    ChordLabel(chord: chord)
                        .font(.title)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 8)
                case nil:
                    Text(
                        isOff
                            ? "The shortcut is off. Turn it on in Settings > Shortcuts."
                            : "Find the shortcut in Settings > Shortcuts."
                    )
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
                NoticeText(notice: state.installNotice)
            }
        }
    }

    /// A binding this build can't read isn't off.
    private var isOff: Bool {
        shortcut.map { $0.binding == nil } ?? false
    }
}
