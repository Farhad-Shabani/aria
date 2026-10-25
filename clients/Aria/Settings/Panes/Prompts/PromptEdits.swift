import Foundation

/// Changes to the prompt list, applied through `PreferencesEditor.editLatest`
/// because the daemon replaces the whole list. A prompt not in the list is
/// left alone.
enum PromptEdits {
    /// The daemon refuses more.
    static let maximumPrompts = 16
    static let kinds: [Aria_Daemon_V1_PromptKind] = [.rewrite, .compose]

    static func entry(_ id: String, in prompts: Aria_Daemon_V1_Prompts) -> Aria_Daemon_V1_Prompt? {
        prompts.entries.first { $0.id == id }
    }

    /// A rewrite, on so the prompt list shows it, with no chord yet.
    static func makeEntry(for file: URL, id: UUID = UUID()) -> Aria_Daemon_V1_Prompt {
        var entry = Aria_Daemon_V1_Prompt()
        entry.id = id.uuidString
        entry.name = file.deletingPathExtension().lastPathComponent
        entry.kind = .rewrite
        entry.file = file.standardizedFileURL.path
        entry.enabled = true
        return entry
    }

    static func remove(_ id: String, from prompts: inout Aria_Daemon_V1_Prompts) {
        prompts.entries.removeAll { $0.id == id }
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

extension PreferencesEditor {
    func editPrompt(_ id: String, _ change: @escaping @MainActor (inout Aria_Daemon_V1_Prompt) -> Void) {
        editLatest(\.prompts, \.prompts) { PromptEdits.update(id, in: &$0, change: change) }
    }
}

extension Aria_Daemon_V1_PromptKind {
    var title: String {
        switch self {
        case .rewrite: return "Rewrite them"
        case .compose: return "Write from them"
        case .unspecified, .UNRECOGNIZED: return "Unknown"
        }
    }
}
