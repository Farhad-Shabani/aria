import SwiftUI

struct PromptsSettings: View {
    let settings: DaemonSettings
    @Binding var selected: String?
    let open: (String) -> Void
    @State private var failure: Failure?

    private struct Failure {
        let title: String
        let reason: PromptDraft.Failure
    }

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
                add: add,
                remove: remove,
                open: open
            ) { entry in
                PromptRow(entry: entry, flagsMissingFile: true, isSelected: selected == entry.id)
            }
        }
        .alert(
            failure?.title ?? "",
            isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } }),
            presenting: failure
        ) { _ in
            Button("OK") {}
        } message: { failure in
            Text(failure.reason.message)
        }
    }

    /// The daemon writes the prompt list. Syncing first lets the new prompt's page find it.
    private func add() {
        guard let file = PromptFile.choose() else { return }
        let request = PromptDraft.saveRequest(adding: file)
        Task {
            switch await changes.save(request) {
            case .success(let saved): open(saved.id)
            case .failure(let reason): failure = Failure(title: "Couldn’t Add Prompt", reason: reason)
            }
        }
    }

    private func remove(_ id: String) {
        Task {
            if let reason = await changes.remove(id) {
                failure = Failure(title: "Couldn’t Remove Prompt", reason: reason)
            }
        }
    }

    private var changes: PromptChanges {
        let settings = settings
        return PromptChanges(refresh: { await settings.sync() })
    }
}
