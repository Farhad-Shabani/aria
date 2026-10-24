import Carbon.HIToolbox

/// What a press of the prompt list chord asks the caller to do.
enum PromptListPress: Equatable {
    /// A run the list started is going, so the chord stands in for that
    /// prompt's own.
    case prompt(id: String)
    case open
    case close
}

/// What the prompt list chord and its number keys mean, decided apart
/// from Carbon and the panel.
struct PromptListGesture: Equatable {
    static let numberKeys = [
        kVK_ANSI_1, kVK_ANSI_2, kVK_ANSI_3, kVK_ANSI_4, kVK_ANSI_5,
        kVK_ANSI_6, kVK_ANSI_7, kVK_ANSI_8, kVK_ANSI_9,
    ]

    private var started: String?

    /// Prompts past the ninth have no key, so the list leaves them out.
    static func keys(for runnable: [Aria_Daemon_V1_Prompt]) -> [(keyCode: Int, entry: Aria_Daemon_V1_Prompt)] {
        zip(numberKeys, runnable).map { (keyCode: $0, entry: $1) }
    }

    /// A run that has ended frees the chord to open the list again.
    mutating func press(isOpen: Bool, isBusy: (String) -> Bool) -> PromptListPress {
        if let started, isBusy(started) {
            return .prompt(id: started)
        }
        started = nil
        return isOpen ? .close : .open
    }

    mutating func chose(_ id: String) {
        started = id
    }
}
