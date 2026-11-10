import Foundation

/// Names a chat: the asks sent under one name. History groups asks by it.
struct ChatKey: Hashable, Sendable {
    /// A chat's name is one line of at most this many bytes.
    static let nameLimit = 128

    let assistantID: Aria_Daemon_V1_AssistantId
    let name: String
}

/// The asks sent under one name, oldest first.
struct Chat: Equatable {
    /// `nil` for a new chat until the daemon names it in the reply to its first ask.
    private(set) var key: ChatKey?
    /// The folder its asks run in, as far as the app knows, for the page to name, a spoken
    /// follow-up's vocabulary, and a follow-up to seed. The daemon keeps the folder each ask runs in.
    let folder: String?
    private(set) var asks: [ChatAsk] = []
    /// The cursor for the saved asks older than these. `nil` once none are left.
    private(set) var older: UInt64?

    init(key: ChatKey?, folder: String?) {
        self.key = key
        self.folder = folder
    }

    /// While its last ask runs, it takes no other.
    var isReplying: Bool { asks.last?.isRunning == true }

    /// Stopping goes through the daemon by its key, so not before the daemon names it.
    var canStop: Bool { key != nil && isReplying }

    /// The history entry `ask` was saved as, which a prompt can run on.
    func entry(of ask: ChatAsk) -> HistoryItem? {
        guard let id = ask.entryID, let reply = ask.reply else { return nil }
        return HistoryItem(
            id: id, kind: .ask(reply: reply), created: ask.created, project: folder, chat: key,
            text: ask.question, resultCount: ask.resultCount, status: .success)
    }

    mutating func append(_ ask: ChatAsk) {
        asks.append(ask)
    }

    /// Adds the saved asks not already here, keyed by their history entry. An ask asked here
    /// stays, since it is the one on screen.
    mutating func merge(_ page: ChatPage) {
        add(page.asks)
        older = page.older
    }

    /// The chat took a new name, which its asks follow.
    mutating func rename(to key: ChatKey) {
        self.key = key
    }

    /// Shows the asks the daemon runs or has waiting, by their ticket: one the stream lost goes
    /// on, and one not here follows the rest. `heard` is what the stream brought while they were
    /// listed, which those asks missed.
    mutating func merge(_ pending: [Aria_Daemon_V1_PendingAsk], heard: [Aria_Daemon_V1_ChatEvent?]) {
        var listed: Set<ChatAsk.ID> = []
        for ask in pending {
            if let index = asks.firstIndex(where: { $0.ticket == ask.ticket }) {
                asks[index].goesOn(ask)
                listed.insert(asks[index].id)
            } else {
                let shown = ChatAsk(pending: ask)
                asks.append(shown)
                listed.insert(shown.id)
            }
        }
        for event in heard {
            for index in asks.indices where listed.contains(asks[index].id) { asks[index].apply(event) }
            dropSavedCopy(repliedIn: event)
        }
    }

    /// The asks the stream lost that `pending` no longer lists, which ended while it was gone.
    func lost(notIn pending: [Aria_Daemon_V1_PendingAsk]) -> Set<ChatAsk.ID> {
        let tickets = Set(pending.map(\.ticket))
        let ended = asks.filter { ask in
            guard ask.isLost, let ticket = ask.ticket else { return false }
            return !tickets.contains(ticket)
        }
        return Set(ended.map(\.id))
    }

    /// Swaps each `ended` ask for its copy among the newest saved asks, matched by question, since
    /// history keeps no ticket. The cursor stays where earlier reads left it.
    mutating func replace(_ ended: Set<ChatAsk.ID>, with page: ChatPage) {
        let known = Set(asks.compactMap(\.entryID))
        var unclaimed = page.asks.filter { !known.contains($0.id) }
        // The daemon saves only an ask that replied. One that failed, was stopped, or died with
        // the daemon has no copy, so it stays lost.
        asks.removeAll { ask in
            guard ended.contains(ask.id), let copy = unclaimed.firstIndex(where: { $0.text == ask.question })
            else { return false }
            unclaimed.remove(at: copy)
            return true
        }
        add(page.asks)
    }

    private mutating func add(_ saved: [HistoryItem]) {
        let known = Set(asks.compactMap(\.entryID))
        let added = saved.filter { !known.contains($0.id) }.map(ChatAsk.init(saved:))
        asks = (asks + added).enumerated()
            .sorted { ($0.element.created, $0.offset) < ($1.element.created, $1.offset) }
            .map(\.element)
    }

    /// A prompt result was saved under `entry`, which may be one of its asks.
    mutating func countResult(on entry: UInt64) {
        guard let index = asks.firstIndex(where: { $0.entryID == entry }) else { return }
        asks[index].countResult()
    }

    /// The daemon named it in the reply to `id`, its first ask.
    mutating func named(_ key: ChatKey, by id: ChatAsk.ID) {
        guard self.key == nil, asks.contains(where: { $0.id == id }) else { return }
        self.key = key
    }

    mutating func started(_ id: ChatAsk.ID, ticket: UInt64) {
        guard let index = asks.firstIndex(where: { $0.id == id }) else { return }
        asks[index].started(ticket: ticket)
        // The daemon may have listed it before its ticket came. The ask asked here holds its events.
        asks.removeAll { $0.id == .pending(ticket) }
    }

    mutating func fail(_ id: ChatAsk.ID, _ message: String) {
        guard let index = asks.firstIndex(where: { $0.id == id }) else { return }
        asks[index].fail(message)
    }

    /// `nil` is the stream going away, and every running ask's reply with it.
    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        for index in asks.indices { asks[index].apply(event) }
        dropSavedCopy(repliedIn: event)
    }

    /// History read before the reply came may already hold the ask that `event` replied to.
    private mutating func dropSavedCopy(repliedIn event: Aria_Daemon_V1_ChatEvent?) {
        guard let event, event.stage == .replied, event.hasHistoryEntryID else { return }
        let entry = event.historyEntryID
        guard asks.contains(where: { $0.entryID == entry && $0.id != .saved(entry) }) else { return }
        asks.removeAll { $0.id == .saved(entry) }
    }
}

/// One question in a chat, and the assistant's reply once it comes.
struct ChatAsk: Identifiable, Equatable {
    enum ID: Hashable {
        /// Read back from history, by its entry.
        case saved(UInt64)
        /// Asked here, numbered in the order asked.
        case live(Int)
        /// Listed by the daemon while it ran, by its ticket.
        case pending(UInt64)
    }

    enum State: Equatable {
        /// What the assistant is doing, such as “Thinking…”.
        case running(String)
        case replied(String)
        case failed(String)
        /// The event stream went away while it ran, so its reply didn't come here.
        case lost
    }

    static let lost = "Aria lost track of this question. Ask again."
    private static let starting = "Starting…"
    static let failed = "Aria couldn’t get a reply. Ask again."

    let id: ID
    let question: String
    let created: Date
    private(set) var state: State
    /// The history entry it was saved as. `nil` while it runs, while history is off, or when the
    /// save failed.
    private(set) var entryID: UInt64?
    /// How many prompt results its entry holds.
    private(set) var resultCount = 0
    private var follow = ChatFollow()

    /// Just sent, before its ticket comes.
    init(live number: Int, question: String, created: Date = .now) {
        id = .live(number)
        self.question = question
        self.created = created
        state = .running(Self.starting)
    }

    /// Listed by the daemon, which runs it or has it waiting.
    init(pending ask: Aria_Daemon_V1_PendingAsk) {
        id = .pending(ask.ticket)
        question = ask.text
        created = .now
        state = .running(Self.step(of: ask))
        _ = follow.started(ticket: ask.ticket)
    }

    init(saved entry: HistoryItem) {
        id = .saved(entry.id)
        question = entry.text
        created = entry.created
        state = .replied(entry.copyText)
        entryID = entry.id
        resultCount = entry.resultCount
    }

    var ticket: UInt64? { follow.ticket }

    var isRunning: Bool {
        if case .running = state { true } else { false }
    }

    var isLost: Bool { state == .lost }

    var reply: String? {
        if case .replied(let text) = state { text } else { nil }
    }

    mutating func countResult() {
        resultCount += 1
    }

    mutating func started(ticket: UInt64) {
        for event in follow.started(ticket: ticket) { take(event) }
    }

    /// The stream lost it, and the daemon still lists it as `ask`.
    mutating func goesOn(_ ask: Aria_Daemon_V1_PendingAsk) {
        guard isLost else { return }
        state = .running(Self.step(of: ask))
    }

    mutating func fail(_ message: String) {
        guard isRunning else { return }
        state = .failed(message)
    }

    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        guard isRunning else { return }
        guard let event else {
            state = .lost
            return
        }
        guard follow.admits(event) else { return }
        take(event)
    }

    private mutating func take(_ event: Aria_Daemon_V1_ChatEvent) {
        guard isRunning else { return }
        switch event.stage {
        case .working:
            state = .running(PendingAsk.title(of: event.step, skill: event.skill))
        case .replied:
            state = .replied(event.text)
            entryID = event.hasHistoryEntryID ? event.historyEntryID : nil
        case .failed:
            state = .failed(Self.message(for: event.errorCode))
        default:
            break
        }
    }

    /// The step a listed ask has reached is unknown until its next event.
    private static func step(of ask: Aria_Daemon_V1_PendingAsk) -> String {
        guard ask.running else { return starting }
        return PendingAsk.title(of: .unspecified, skill: "")
    }

    static func message(for code: Aria_Daemon_V1_ErrorCode) -> String {
        switch code {
        case .assistantSignedOut: "The assistant is signed out. Sign in, then ask again."
        case .assistantUnavailable: "The assistant isn’t installed. Choose one in Settings."
        case .assistantTimeout: "The assistant took too long to reply. Ask again."
        case .busy: "Aria is busy. Ask again in a moment."
        case .cancelled: "You stopped this question."
        default: failed
        }
    }
}
