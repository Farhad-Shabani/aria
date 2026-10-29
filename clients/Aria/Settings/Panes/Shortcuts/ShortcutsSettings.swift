import SwiftUI

struct ShortcutsSettings: View {
    let preferences: DaemonPreferences
    let askChord: ChordBinding
    let promptListChord: ChordBinding
    let openPrompt: (String) -> Void

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            let ariaKey = editor.snapshot.storedDictationHotkey ?? .default
            Section {
                Picker(
                    "Aria key",
                    selection: editor.binding(ariaKey) { key, edit in
                        edit.dictationHotkey = key.wire
                    }
                ) {
                    ForEach(HotkeyKey.allCases, id: \.self) { key in
                        Text(key.title).tag(key)
                    }
                }
                // An unknown stored key stays untouched rather than shown as Right Option.
                .disabled(editor.snapshot.storedDictationHotkey == nil)
                LabeledContent("Dictate", value: "Tap, or hold to talk")
                LabeledContent("Ask", value: ariaKey.gesture(for: .ask))
                LabeledContent("Show prompt list", value: ariaKey.gesture(for: .promptList))
            } footer: {
                Text(
                    "Hold the Aria key, tap Shift or Command, then let go. Tap the Aria key alone to send, and again to cancel. Needs Accessibility access, and `hotkey.enabled` left on in the daemon config."
                )
            }
            Section("Ask") {
                HotkeyRecorder(
                    title: "Shortcut",
                    chord: editor.snapshot.askHotkey.storedChord,
                    binding: askChord
                ) {
                    editor.set(\.askHotkey, to: Aria_Daemon_V1_Hotkey($0))
                }
                // Nothing to edit until the daemon sends its ask hotkey.
                .disabled(!editor.snapshot.hasAskHotkey)
            }
            Section("Prompts") {
                HotkeyRecorder(
                    title: "Prompt list shortcut",
                    chord: editor.snapshot.promptListHotkey.storedChord,
                    binding: promptListChord
                ) {
                    editor.set(\.promptListHotkey, to: Aria_Daemon_V1_Hotkey($0))
                }
                .disabled(!editor.snapshot.hasPromptListHotkey)
                ForEach(editor.snapshot.prompts.entries, id: \.id) { entry in
                    ItemLink {
                        openPrompt(entry.id)
                    } label: {
                        PromptRow(entry: entry)
                    }
                }
            }
        }
    }
}
