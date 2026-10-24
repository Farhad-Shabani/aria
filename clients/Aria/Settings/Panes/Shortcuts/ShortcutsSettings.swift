import SwiftUI

struct ShortcutsSettings: View {
    let preferences: DaemonPreferences
    let askChord: ChordBinding
    let promptListChord: ChordBinding
    let openPrompt: (String) -> Void

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            Section("Dictation") {
                Picker(
                    "Dictation hotkey",
                    selection: editor.binding(editor.snapshot.storedDictationHotkey ?? .default) { key, edit in
                        edit.dictationHotkey = key.wire
                    }
                ) {
                    ForEach(HotkeyKey.allCases, id: \.self) { key in
                        Text(key.title).tag(key)
                    }
                }
                // An unknown stored key stays untouched rather than shown as Right Option.
                .disabled(editor.snapshot.storedDictationHotkey == nil)
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
                    title: "Show prompt list",
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
