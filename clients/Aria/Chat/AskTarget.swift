enum AskTarget: Equatable {
    case new
    case followUp(ChatKey)

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
