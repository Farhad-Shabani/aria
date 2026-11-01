import SwiftUI

struct PromptsSettings: View {
    let settings: DaemonSettings
    @Binding var selected: String?
    let open: (String) -> Void

    var body: some View {
        SettingsContent(settings: settings) { editor in
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
                PromptRow(entry: entry, flagsMissingFile: true, isSelected: selected == entry.id)
            }
        }
    }

    private func add(_ editor: SettingsEditor) {
        guard let file = PromptFile.choose() else { return }
        let entry = PromptEdits.makeEntry(for: file)
        editor.editLatest(\.prompts, \.prompts) { PromptEdits.add(entry, to: &$0) }
        open(entry.id)
    }
}
