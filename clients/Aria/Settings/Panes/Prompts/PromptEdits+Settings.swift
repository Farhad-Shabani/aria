extension PromptEdits {
    static func entry(_ id: String, in prompts: Aria_Daemon_V1_Prompts) -> Aria_Daemon_V1_Prompt? {
        prompts.entries.first { $0.id == id }
    }

    static func update(
        _ id: String,
        in prompts: inout Aria_Daemon_V1_Prompts,
        change: (inout Aria_Daemon_V1_Prompt) -> Void
    ) {
        guard let index = prompts.entries.firstIndex(where: { $0.id == id }) else { return }
        change(&prompts.entries[index])
    }
}

extension SettingsEditor {
    /// Goes through `editLatest` because the daemon replaces the whole list.
    func editPrompt(_ id: String, _ change: @escaping @MainActor (inout Aria_Daemon_V1_Prompt) -> Void) {
        editLatest(\.prompts, \.prompts) { PromptEdits.update(id, in: &$0, change: change) }
    }
}
