import SwiftUI

struct PromptsSettings: View {
    let preferences: DaemonPreferences
    @Binding var selected: String?
    let open: (String) -> Void

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            let prompts = editor.snapshot.prompts
            ItemList(
                items: prompts.entries,
                id: \.id,
                noun: "Prompt",
                empty: "No Prompts",
                selected: $selected,
                canAdd: prompts.entries.count < PromptEdits.maximumPrompts,
                add: { add(editor) },
                remove: { id in
                    editor.editLatest(\.prompts, \.prompts) { PromptEdits.remove(id, from: &$0) }
                },
                open: open
            ) { entry in
                PromptRow(entry: entry, flagsMissingFile: true)
            }
        }
    }

    private func add(_ editor: PreferencesEditor) {
        guard let file = PromptFile.choose() else { return }
        let entry = PromptEdits.makeEntry(for: file)
        editor.editLatest(\.prompts, \.prompts) { $0.entries.append(entry) }
        open(entry.id)
    }
}
