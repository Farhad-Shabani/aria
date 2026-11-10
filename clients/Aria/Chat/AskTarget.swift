/// Where the Ask shortcut sends the question it records, decided as it is pressed.
enum AskTarget: Equatable {
    /// No chat shows, so the question starts one.
    case new
    /// The question follows up in the chat that shows.
    case followUp(ChatKey)

    /// `nil` while the chat that shows is replying, since it takes no question until then.
    /// One the daemon never named has nothing to follow up in, so the question starts another.
    init?(shown: Chat?) {
        guard let shown else {
            self = .new
            return
        }
        guard !shown.isReplying else { return nil }
        guard let key = shown.key else {
            self = .new
            return
        }
        self = .followUp(key)
    }
}
