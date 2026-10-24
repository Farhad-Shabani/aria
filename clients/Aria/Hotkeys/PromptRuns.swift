/// What the daemon names a run by: its recording, then its ticket.
enum RunID: Hashable {
    case recording(UInt64)
    case ticket(UInt64)
}

/// The prompt behind each run a prompt started, by name, so the status pill
/// can say which prompt is running.
struct PromptRuns: Equatable {
    private struct Run: Equatable {
        let id: RunID
        let prompt: String
    }

    // Keeps the latest few instead of retiring a run when it ends. A run
    // older than these has long left the pill.
    private static let kept = 16

    private var runs: [Run] = []

    mutating func add(_ id: RunID, prompt: String) {
        runs.append(Run(id: id, prompt: prompt))
        runs.removeFirst(max(0, runs.count - Self.kept))
    }

    func prompt(of id: RunID) -> String? {
        runs.last { $0.id == id }?.prompt
    }
}
