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
            } footer: {
                Text(
                    "Tap to dictate, or hold to talk. Requires Accessibility access."
                )
            }
            Section {
                HotkeyRecorder(
                    title: "Shortcut",
                    hotkey: editor.snapshot.askHotkey,
                    ariaKey: ariaKey,
                    binding: askChord
                ) {
                    editor.set(\.askHotkey, to: Aria_Daemon_V1_Hotkey($0))
                }
                // Nothing to edit until the daemon sends its ask hotkey.
                .disabled(!editor.snapshot.hasAskHotkey)
            } header: {
                Text("Ask")
            } footer: {
                Text("Tap the Aria key to send. Tap again to cancel.")
            }
            Section("Prompts") {
                HotkeyRecorder(
                    title: "Prompt list shortcut",
                    hotkey: editor.snapshot.promptListHotkey,
                    ariaKey: ariaKey,
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
            HStack {
                Spacer()
                Button("Restore Defaults") {
                    editor.set(()) { _, edit in
                        edit.askHotkey = Aria_Daemon_V1_Hotkey(.combo(.shift))
                        edit.promptListHotkey = Aria_Daemon_V1_Hotkey(.combo(.command))
                    }
                }
                .disabled(editor.snapshot.holdsDefaultShortcuts)
            }
        }
    }
}

extension Aria_Daemon_V1_UserPreferences {
    fileprivate var holdsDefaultShortcuts: Bool {
        askHotkey.storedBinding == .combo(.shift) && promptListHotkey.storedBinding == .combo(.command)
    }
}
