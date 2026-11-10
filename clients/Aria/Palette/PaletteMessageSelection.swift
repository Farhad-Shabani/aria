/// One side of an ask on the chat page: what was asked, or what came back.
struct ChatMessage: Hashable {
    enum Side: Hashable {
        case question, reply
    }

    let ask: ChatAsk.ID
    let side: Side

    /// Each ask's question, then its reply, oldest first.
    static func messages(in asks: [ChatAsk]) -> [ChatMessage] {
        asks.flatMap {
            [ChatMessage(ask: $0.id, side: .question), ChatMessage(ask: $0.id, side: .reply)]
        }
    }
}

/// The messages the keys rest on: a run from `anchor` to `cursor`, as ⇧ and the arrows extend a
/// list's selection. Until the keys move, it rests on the newest reply and follows the asks that
/// come.
struct MessageSelection: Equatable {
    private var anchor: ChatMessage?
    private var cursor: ChatMessage?

    func selected(in asks: [ChatAsk]) -> [ChatMessage] {
        let messages = ChatMessage.messages(in: asks)
        guard let last = messages.indices.last else { return [] }
        let end = index(of: cursor, in: messages) ?? last
        let start = index(of: anchor, in: messages) ?? end
        return Array(messages[min(start, end)...max(start, end)])
    }

    /// Steps one message, stopping at either end. Extending keeps the anchor, else it follows.
    mutating func move(by step: Int, extending: Bool, in asks: [ChatAsk]) {
        let messages = ChatMessage.messages(in: asks)
        guard let last = messages.indices.last else { return }
        let from = index(of: cursor, in: messages) ?? last
        let start = index(of: anchor, in: messages) ?? from
        let to = min(max(from + step, 0), last)
        cursor = messages[to]
        anchor = extending ? messages[start] : cursor
    }

    mutating func selectAll(in asks: [ChatAsk]) {
        let messages = ChatMessage.messages(in: asks)
        anchor = messages.first
        cursor = messages.last
    }

    /// Back to the cursor alone. Returns whether more than that was selected.
    mutating func collapse(in asks: [ChatAsk]) -> Bool {
        guard selected(in: asks).count > 1 else { return false }
        anchor = cursor
        return true
    }

    /// Resting on the newest reply, as a page opens, before the keys move.
    var isResting: Bool { anchor == nil && cursor == nil }

    /// The message the keys moved to last.
    func focused(in asks: [ChatAsk]) -> ChatMessage? {
        let messages = ChatMessage.messages(in: asks)
        return index(of: cursor, in: messages).map { messages[$0] } ?? messages.last
    }

    /// The ask under the cursor, which a prompt runs on.
    func focusedAsk(in asks: [ChatAsk]) -> ChatAsk? {
        guard let message = focused(in: asks) else { return nil }
        return asks.first { $0.id == message.ask }
    }

    /// The selected messages' text, in order, a blank line apart: a question as asked, and a reply
    /// once it has come. `nil` while none of them has text.
    func text(in asks: [ChatAsk]) -> String? {
        let parts = selected(in: asks).compactMap { message -> String? in
            guard let ask = asks.first(where: { $0.id == message.ask }) else { return nil }
            return message.side == .question ? ask.question : ask.reply
        }
        return parts.isEmpty ? nil : parts.joined(separator: "\n\n")
    }

    private func index(of message: ChatMessage?, in messages: [ChatMessage]) -> Int? {
        message.flatMap { messages.firstIndex(of: $0) }
    }
}
