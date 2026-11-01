import SwiftUI

struct ShortcutsSettings: View {
    let settings: DaemonSettings
    let askChord: ChordBinding
    let promptListChord: ChordBinding
    let openPrompt: (String) -> Void

    var body: some View {
        SettingsForm(settings: settings) { editor in
            let ariaKey = editor.snapshot.storedAriaKey ?? .default
            Section {
                Picker(
                    "Aria key",
                    selection: editor.binding(ariaKey) { key, edit in
                        edit.ariaKey = key.wire
                    }
                ) {
                    ForEach(AriaKey.allCases, id: \.self) { key in
                        Text(key.title).tag(key)
                    }
                }
                // An unknown stored key stays untouched rather than shown as Right Option.
                .disabled(editor.snapshot.storedAriaKey == nil)
            } footer: {
                Text("Tap to dictate, or hold to talk. Requires permission in Privacy & Security.")
            }
            Section {
                HotkeyRecorder(
                    title: "Shortcut",
                    hotkey: editor.snapshot.askShortcut,
                    ariaKey: ariaKey,
                    binding: askChord,
                    check: { chord in try await editor.check { $0.askShortcut = Aria_Daemon_V1_Shortcut(.chord(chord)) }
                    }
                ) {
                    editor.set(\.askShortcut, to: Aria_Daemon_V1_Shortcut($0))
                }
                // Nothing to edit until the daemon sends its ask hotkey.
                .disabled(!editor.snapshot.hasAskShortcut)
            } header: {
                Text("Ask")
            } footer: {
                Text("Tap the Aria key to send. Tap again to cancel.")
            }
            Section("Prompts") {
                HotkeyRecorder(
                    title: "Prompt list shortcut",
                    hotkey: editor.snapshot.promptListShortcut,
                    ariaKey: ariaKey,
                    binding: promptListChord,
                    check: { chord in
                        try await editor.check { $0.promptListShortcut = Aria_Daemon_V1_Shortcut(.chord(chord)) }
                    }
                ) {
                    editor.set(\.promptListShortcut, to: Aria_Daemon_V1_Shortcut($0))
                }
                .disabled(!editor.snapshot.hasPromptListShortcut)
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
                        edit.askShortcut = Aria_Daemon_V1_Shortcut(.combo(.shift))
                        edit.promptListShortcut = Aria_Daemon_V1_Shortcut(.combo(.command))
                    }
                }
                .disabled(editor.snapshot.holdsDefaultShortcuts)
            }
        }
    }
}

extension Aria_Daemon_V1_Settings {
    fileprivate var holdsDefaultShortcuts: Bool {
        askShortcut.storedBinding == .combo(.shift) && promptListShortcut.storedBinding == .combo(.command)
    }
}
