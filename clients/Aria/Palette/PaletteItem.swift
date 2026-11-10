/// One row on the commands page: a command, a history entry, a chat, or a prompt
/// result in an open thread.
enum PaletteItem: Identifiable, Equatable {
    case command(PaletteCommand)
    case entry(HistoryItem)
    case chat(SavedChat)
    case result(PromptResult)

    var id: String {
        switch self {
        case .command(let command): command.id
        case .entry(let entry): "entry-\(entry.id)"
        // Its newest ask changes as it goes on. The selection stays on it.
        case .chat(let chat):
            "chat-\(chat.key.assistantID.rawValue)-\(chat.key.name)"
        case .result(let result): "result-\(result.id)"
        }
    }

    /// Read aloud when the selection moves, since focus stays in the field.
    var spokenTitle: String {
        switch self {
        case .command(let command): command.title
        case .entry(let entry) where entry.resultCount > 0:
            "\(entry.name), \(PromptResult.countLabel(entry.resultCount))"
        case .entry(let entry): entry.name
        case .chat(let chat): "\(chat.name), chat"
        case .result(let result): "\(result.promptName) result"
        }
    }

    /// Saved text Return can type into the app in front: a chat's is its newest reply. A
    /// command runs instead.
    var isTypable: Bool {
        switch self {
        case .entry(let entry): entry.canCopy
        case .chat(let chat): chat.newest.canCopy
        case .result: true
        case .command: false
        }
    }

    var command: PaletteCommand? {
        if case .command(let command) = self { command } else { nil }
    }

    var entry: HistoryItem? {
        if case .entry(let entry) = self { entry } else { nil }
    }

    var result: PromptResult? {
        if case .result(let result) = self { result } else { nil }
    }

    /// The saved item the row stands for: an entry, or a chat's newest ask.
    var historyItem: HistoryItem? {
        switch self {
        case .entry(let entry): entry
        case .chat(let chat): chat.newest
        case .command, .result: nil
        }
    }
}

/// A chat as history lists it: once, at its newest ask.
struct SavedChat: Equatable {
    let key: ChatKey
    let newest: HistoryItem

    /// `nil` for an entry in no chat.
    init?(newest: HistoryItem) {
        guard let key = newest.chat else { return nil }
        self.key = key
        self.newest = newest
    }

    var name: String { key.name }
}
