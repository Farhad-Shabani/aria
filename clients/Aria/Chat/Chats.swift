import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Chats")

/// The chat the palette shows, one at a time, and the chats closed while they reply. An ask goes
/// on in the daemon while its chat is closed, and shows again when the chat reopens.
@MainActor
final class Chats: ObservableObject {
    struct Effects {
        /// The folder and assistant a chat started from the palette runs with.
        var resolve: () async throws -> ChatTarget
        /// Sends a new chat's first question, with any quote, typing nothing. The daemon names
        /// the chat.
        var start:
            (_ text: String, _ quote: String?, _ target: ChatTarget, _ ownVoice: Bool) async throws ->
                StartedChat
        /// Sends a question in a chat, typing nothing, and returns its ticket. The daemon
        /// runs it in the folder it recorded for the chat, or in `folder` when it has none.
        var ask:
            (_ text: String, _ chat: ChatKey, _ folder: String?, _ ownVoice: Bool, _ tapCancels: Bool)
                async throws -> UInt64
        /// Stops a chat's running ask and every one waiting behind it.
        var cancel: (ChatKey) async throws -> Void
        /// A chat's newest saved asks before a cursor.
        var read: (ChatKey, _ before: UInt64?) async throws -> ChatPage
        /// A chat's asks the daemon runs or has waiting, in the order it runs them.
        var pending: (ChatKey) async throws -> [Aria_Daemon_V1_PendingAsk]
    }

    /// The shown chat and the chats closed while an ask in them runs. A closed one whose ask the
    /// stream lost stays until the daemon lists its asks again. Each goes once none runs.
    @Published private(set) var followed: [Chat] = []
    @Published private var shownID: Chat.ID?
    /// The chat a spoken follow-up is being recorded for.
    @Published var recording: ChatKey?
    /// Why the shown chat's saved asks couldn't be read.
    @Published private(set) var error: HistoryFailure?
    /// The last Stop, until the chat it was for stops replying or another chat shows.
    @Published private(set) var lastStop: Stop?
    /// How the last off-page chat's ask ended, for VoiceOver on the list.
    @Published private(set) var lastOutcome: Outcome?
    private var outcomes = 0
    /// While off, nothing is read from history: a chat holds what was asked here and the
    /// asks the daemon still runs.
    var keepsHistory = false
    /// The cursor of the last read, which Try Again reads again. `nil` reads the newest asks.
    private var lastRead: UInt64?
    /// Numbers the asks asked here, across chats, so a late ticket finds only its own.
    private var asked = 0
    private var made = 0
    private var reading: Task<Void, Never>?
    private var rereading: Task<Void, Never>?
    private var listings: [ChatKey: Task<Void, Never>] = [:]
    /// Events the stream brought during each chat's pending listing, which the listed asks missed.
    private var heard: [ChatKey: [Aria_Daemon_V1_ChatEvent?]] = [:]
    /// Whether the event stream is up. While it's down, a lost ask can't be listed yet.
    private var isConnected = true
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    var shown: Chat? { followed.first { $0.id == shownID } }

    /// Shows the new chat at once. The chat takes its key from the daemon's reply.
    func startNew(
        _ question: String, quoting quote: String? = nil, from entry: UInt64? = nil, at target: ChatTarget,
        ownVoice: Bool
    ) {
        show(makeChat(key: nil, folder: target.workdir, quote: quote))
        let id = append(question, from: entry)
        Task {
            do {
                let started = try await effects.start(question, quote, target, ownVoice)
                update(id) { chat in
                    chat.named(started.chat, by: id)
                    chat.started(id, ticket: started.ticket)
                }
            } catch {
                fail(id, error)
            }
        }
    }

    /// Starts a chat quoting `quote`, the text of history entry `entry`. It shows once the daemon
    /// picks a folder, or fails to.
    func startNew(_ text: String, quoting quote: String, from entry: UInt64) async {
        let question = Self.question(text)
        let target: ChatTarget
        do {
            target = try await effects.resolve()
        } catch {
            guard !Task.isCancelled else { return }
            show(makeChat(key: nil, folder: nil, quote: quote))
            fail(append(question, from: entry), error)
            return
        }
        guard !Task.isCancelled else { return }
        startNew(question, quoting: quote, from: entry, at: target, ownVoice: false)
    }

    /// A spoken follow-up. The chat may have closed during recording, so it opens again.
    func followUp(_ question: String, in chat: ChatKey, folder: String?, ownVoice: Bool) {
        open(chat, folder: folder)
        ask(question, ownVoice: ownVoice, tapCancels: true)
    }

    var takesFollowUp: Bool {
        guard let shown, let key = shown.key else { return false }
        return !shown.isReplying && recording != key
    }

    func canSend(_ text: String) -> Bool {
        takesFollowUp && !Self.question(text).isEmpty
    }

    func send(_ text: String) -> Bool {
        guard canSend(text) else { return false }
        ask(Self.question(text), ownVoice: false, tapCancels: false)
        return true
    }

    private static func question(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    func open(_ chat: ChatKey, folder: String?) {
        guard shown?.key != chat else { return }
        load(find(chat) ?? makeChat(key: chat, folder: folder))
    }

    /// Shows the chat replying to a quote of `entry`, which may have no name yet. False when none
    /// does, such as once the reply came.
    func open(quoting entry: UInt64) -> Bool {
        guard let chat = replying(quoting: entry) else { return false }
        if chat.id != shownID { load(chat) }
        return true
    }

    func replying(_ chat: ChatKey) -> Chat? {
        find(chat).flatMap { $0.isReplying ? $0 : nil }
    }

    /// The chat whose ask quoting history entry `entry` still runs, for that entry's row.
    func replying(quoting entry: UInt64) -> Chat? {
        followed.first { $0.isReplying(quoting: entry) }
    }

    func readEarlier() {
        guard reading == nil, error == nil, let older = shown?.older else { return }
        read(before: older)
    }

    func retryRead() {
        guard reading == nil, error != nil, shown != nil else { return }
        error = nil
        read(before: lastRead)
    }

    /// Stops a chat's running ask and every ask waiting behind it, whether its page shows or not.
    /// The daemon saves none of their replies.
    func stop(_ key: ChatKey) {
        guard find(key)?.canStop == true else { return }
        lastStop = Stop(chat: key)
        Task {
            do {
                try await effects.cancel(key)
            } catch {
                log.error("Stopping a chat failed: \(String(describing: error), privacy: .private)")
                guard lastStop?.chat == key, find(key)?.isReplying == true else { return }
                lastStop?.failure =
                    (error as? DaemonFailure)?.isOffline == true
                    ? DaemonFailure.offlineMessage : Self.stopFailed
            }
        }
    }

    func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        if event == nil { isConnected = false }
        for key in heard.keys { heard[key]?.append(event) }
        for index in followed.indices { followed[index].apply(event) }
        settle()
    }

    private func find(_ key: ChatKey) -> Chat? {
        followed.first { $0.key == key }
    }

    /// The event stream replays nothing after it reconnects, so each chat's lost asks get
    /// listed again. One the daemon still runs goes on. One that ended meanwhile is read from
    /// history.
    func reconnected() {
        isConnected = true
        for key in followed.compactMap(\.key) { listPending(key) }
    }

    /// History deleted `chat`, so nothing of it stays here.
    func deleted(_ chat: ChatKey) {
        followed.removeAll { $0.key == chat && $0.id != shownID }
        if lastStop?.chat == chat { lastStop = nil }
    }

    func renamed(_ chat: ChatKey, to renamed: ChatKey) {
        for index in followed.indices where followed[index].key == chat { followed[index].rename(to: renamed) }
        if recording == chat { recording = renamed }
    }

    func countResult(on entry: UInt64) {
        changeShown { $0.countResult(on: entry) }
    }

    func shows(ticket: UInt64) -> Bool {
        shown?.asks.contains { $0.ticket == ticket } ?? false
    }

    func close() {
        for task in [reading, rereading] { task?.cancel() }
        reading = nil
        rereading = nil
        let leaving = shown
        shownID = nil
        if let leaving, !staysOffPage(leaving) {
            followed.removeAll { $0.id == leaving.id }
            if let key = leaving.key { stopListing(key) }
        }
        error = nil
        settle()
    }

    private func makeChat(key: ChatKey?, folder: String?, quote: String? = nil) -> Chat {
        made += 1
        return Chat(id: made, key: key, folder: folder, quote: quote)
    }

    private func show(_ chat: Chat) {
        close()
        if !followed.contains(where: { $0.id == chat.id }) { followed.append(chat) }
        shownID = chat.id
        // A failure shows only where its chat does: on that chat's page, or on the list.
        if let stop = lastStop, stop.chat != chat.key { lastStop = nil }
    }

    /// Shows `chat`, then reads its saved asks and lists those the daemon runs.
    private func load(_ chat: Chat) {
        show(chat)
        if keepsHistory { read(before: nil) }
        if let key = chat.key { listPending(key) }
    }

    /// A closed chat stays while an ask in it runs, or while its lost ask waits to be listed.
    private func staysOffPage(_ chat: Chat) -> Bool {
        if chat.isReplying { return true }
        guard chat.hasLostAsk else { return false }
        guard let key = chat.key else { return true }
        return !isConnected || listings[key] != nil
    }

    /// Drops the closed chats nothing runs in, and a Stop failure once its chat stops replying.
    private func settle() {
        let ended = followed.filter { $0.id != shownID && !staysOffPage($0) }
        let endedIDs = Set(ended.map(\.id))
        followed.removeAll { endedIDs.contains($0.id) }
        if let announcement = ended.last?.asks.last?.state.announcement {
            outcomes += 1
            lastOutcome = Outcome(number: outcomes, announcement: announcement)
        }
        if let stop = lastStop, find(stop.chat)?.isReplying != true { lastStop = nil }
    }

    private func ask(_ question: String, ownVoice: Bool, tapCancels: Bool) {
        guard let key = shown?.key else { return }
        let folder = shown?.folder
        let id = append(question)
        Task {
            do {
                let ticket = try await effects.ask(question, key, folder, ownVoice, tapCancels)
                update(id) { $0.started(id, ticket: ticket) }
            } catch {
                fail(id, error)
            }
        }
    }

    private func append(_ question: String, from entry: UInt64? = nil) -> ChatAsk.ID {
        asked += 1
        changeShown { $0.append(ChatAsk(live: asked, question: question, from: entry)) }
        return .live(asked)
    }

    private func fail(_ id: ChatAsk.ID, _ error: Error) {
        update(id) { $0.fail(id, Self.message(for: error)) }
    }

    private func changeShown(_ change: (inout Chat) -> Void) {
        guard let index = followed.firstIndex(where: { $0.id == shownID }) else { return }
        change(&followed[index])
    }

    /// Changes the chat that holds the ask `id`, shown or closed while it replies.
    private func update(_ id: ChatAsk.ID, _ change: (inout Chat) -> Void) {
        guard let index = followed.firstIndex(where: { $0.asks.contains { $0.id == id } }) else { return }
        change(&followed[index])
        let changed = followed[index]
        // Named after the stream went away, so its events were missed.
        if changed.hasLostAsk, isConnected, let key = changed.key, listings[key] == nil { listPending(key) }
        settle()
    }

    private func listPending(_ key: ChatKey) {
        listings[key]?.cancel()
        heard[key] = []
        listings[key] = Task {
            do {
                let pending = try await effects.pending(key)
                guard !Task.isCancelled else { return }
                stopListing(key, listed: pending)
            } catch {
                guard !Task.isCancelled else { return }
                log.error("Listing a chat's asks failed: \(String(describing: error), privacy: .private)")
                stopListing(key)
            }
        }
    }

    /// Ends `key`'s listing, merging the asks it listed into that chat.
    private func stopListing(_ key: ChatKey, listed pending: [Aria_Daemon_V1_PendingAsk]? = nil) {
        let heard = self.heard[key] ?? []
        listings.removeValue(forKey: key)?.cancel()
        self.heard[key] = nil
        guard let pending, let index = followed.firstIndex(where: { $0.key == key }) else { return settle() }
        let ended = followed[index].lost(notIn: pending)
        followed[index].merge(pending, heard: heard)
        if followed[index].id == shownID, keepsHistory, !ended.isEmpty { reread(replacing: ended) }
        settle()
    }

    private func reread(replacing ended: Set<ChatAsk.ID>) {
        guard let key = shown?.key else { return }
        rereading?.cancel()
        rereading = Task {
            defer { if !Task.isCancelled { rereading = nil } }
            do {
                let page = try await effects.read(key, nil)
                guard !Task.isCancelled, shown?.key == key else { return }
                changeShown { $0.replace(ended, with: page) }
            } catch {
                guard !Task.isCancelled else { return }
                log.error("Reading a chat again failed: \(String(describing: error), privacy: .private)")
            }
        }
    }

    private func read(before: UInt64?) {
        guard let key = shown?.key else { return }
        lastRead = before
        reading = Task {
            defer { if !Task.isCancelled { reading = nil } }
            do {
                let page = try await effects.read(key, before)
                guard !Task.isCancelled, shown?.key == key else { return }
                changeShown { $0.merge(page) }
            } catch {
                guard !Task.isCancelled, shown?.key == key else { return }
                // Nothing saved yet, such as an ask still replying in a chat just begun.
                if error as? HistoryFailure == .missing { return }
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    static let stopFailed = "Aria couldn’t stop this question. Try again."

    /// An off-page chat's ask outcome, numbered so the same words announce again.
    struct Outcome: Equatable {
        let number: Int
        let announcement: String
    }

    /// A Stop sent for `chat`, with why it didn't reach the daemon once it fails.
    struct Stop: Equatable {
        let chat: ChatKey
        var failure: String?
    }

    private static func message(for error: Error) -> String {
        let failure = error as? DaemonFailure
        if failure?.isOffline == true { return DaemonFailure.offlineMessage }
        if case .refused(let code)? = failure { return ChatAsk.message(for: code) }
        return ChatAsk.failed
    }
}

extension Chats.Effects {
    static var live: Self {
        .init(
            resolve: { try await ChatClient.resolve() },
            start: { try await ChatClient.start(text: $0, quoting: $1, at: $2, ownVoice: $3) },
            ask: { try await ChatClient.ask(text: $0, in: $1, folder: $2, ownVoice: $3, tapCancels: $4) },
            cancel: { try await ChatClient.cancel(chat: $0) },
            read: { try await HistoryClient.chat($0, before: $1) },
            pending: { try await ChatClient.pendingAsks(in: $0) })
    }
}
