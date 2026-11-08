/// One row on the commands page: a command, a history entry, or a prompt
/// result in an open thread.
enum PaletteItem: Identifiable, Equatable {
    case command(PaletteCommand)
    case entry(HistoryItem)
    case result(PromptResult)

    var id: String {
        switch self {
        case .command(let command): command.id
        case .entry(let entry): "entry-\(entry.id)"
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
        case .result(let result): "\(result.promptName) result"
        }
    }

    /// Saved text Return can type into the app in front. A command runs instead.
    var isTypable: Bool {
        switch self {
        case .entry(let entry): entry.canCopy
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
}
