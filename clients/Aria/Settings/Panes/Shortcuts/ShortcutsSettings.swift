import SwiftUI

struct ShortcutsSettings: View {
    let preferences: DaemonPreferences

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            Section("Dictation") {
                Picker(
                    "Dictation hotkey",
                    selection: editor.binding(editor.snapshot.storedDictationHotkey ?? .rightOption) { key, edit in
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
                AskHotkeyRecorder(editor: editor)
            }
        }
    }
}
