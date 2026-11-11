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

    @Published private(set) var shown: Chat?
    /// Chats closed while an ask in them runs. One whose ask the stream lost stays until the daemon
    /// lists its asks again. Each goes once none runs.
    @Published private(set) var offPage: [Chat] = []
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
    private var reading: Task<Void, Never>?
    private var rereading: Task<Void, Never>?
    private var listings: [ChatKey: Task<Void, Never>] = [:]
    /// Events the stream brought during each chat's pending listing, which the listed asks missed.
    private var heard: [ChatKey: [Aria_Daemon_V1_ChatEvent?]] = [:]
    /// Whether the event stream is up. While it's down, a lost ask can't be listed yet.
    private var isConnected = true
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    /// Shows the new chat at once. The chat takes its key from the daemon's reply.
    func startNew(
        _ question: String, quoting quote: String? = nil, from entry: UInt64? = nil, at target: ChatTarget,
        ownVoice: Bool
    ) {
        show(Chat(key: nil, folder: target.workdir, quote: quote))
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
            show(Chat(key: nil, folder: nil, quote: quote))
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
        let index = offPage.firstIndex { $0.key == chat }
        load(index.map { offPage.remove(at: $0) } ?? Chat(key: chat, folder: folder))
    }

    /// Shows the chat replying to a quote of `entry`, which may have no name yet. False when none
    /// does, such as once the reply came.
    func open(quoting entry: UInt64) -> Bool {
        if shown?.isReplying(quoting: entry) == true { return true }
        guard let index = offPage.firstIndex(where: { $0.isReplying(quoting: entry) }) else { return false }
        load(offPage.remove(at: index))
        return true
    }

    func isReplying(_ chat: ChatKey) -> Bool {
        find(chat)?.isReplying == true
    }

    func isReplying(quoting entry: UInt64) -> Bool {
        shown?.isReplying(quoting: entry) == true || offPage.contains { $0.isReplying(quoting: entry) }
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
        shown?.apply(event)
        for index in offPage.indices { offPage[index].apply(event) }
        settle()
    }

    private func find(_ key: ChatKey) -> Chat? {
        shown?.key == key ? shown : offPage.first { $0.key == key }
    }

    /// The event stream replays nothing after it reconnects, so each chat's lost asks get
    /// listed again. One the daemon still runs goes on. One that ended meanwhile is read from
    /// history.
    func reconnected() {
        isConnected = true
        for key in ([shown?.key] + offPage.map(\.key)).compactMap({ $0 }) { listPending(key) }
    }

    /// History deleted `chat`, so nothing of it stays here.
    func deleted(_ chat: ChatKey) {
        offPage.removeAll { $0.key == chat }
        if lastStop?.chat == chat { lastStop = nil }
    }

    func renamed(_ chat: ChatKey, to renamed: ChatKey) {
        if shown?.key == chat { shown?.rename(to: renamed) }
        if recording == chat { recording = renamed }
    }

    func countResult(on entry: UInt64) {
        shown?.countResult(on: entry)
    }

    func shows(ticket: UInt64) -> Bool {
        shown?.asks.contains { $0.ticket == ticket } ?? false
    }

    func close() {
        for task in [reading, rereading] { task?.cancel() }
        reading = nil
        rereading = nil
        if let shown, staysOffPage(shown) {
            offPage.append(shown)
        } else if let key = shown?.key {
            stopListing(key)
        }
        shown = nil
        error = nil
        settle()
    }

    private func show(_ chat: Chat) {
        close()
        shown = chat
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
        let ended = offPage.filter { !staysOffPage($0) }
        offPage.removeAll { !staysOffPage($0) }
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
        shown?.append(ChatAsk(live: asked, question: question, from: entry))
        return .live(asked)
    }

    private func fail(_ id: ChatAsk.ID, _ error: Error) {
        update(id) { $0.fail(id, Self.message(for: error)) }
    }

    /// Changes the chat that holds the ask `id`, shown or closed while it replies.
    private func update(_ id: ChatAsk.ID, _ change: (inout Chat) -> Void) {
        let changed: Chat
        if var chat = shown, chat.asks.contains(where: { $0.id == id }) {
            change(&chat)
            shown = chat
            changed = chat
        } else if let index = offPage.firstIndex(where: { $0.asks.contains { $0.id == id } }) {
            change(&offPage[index])
            changed = offPage[index]
        } else {
            return
        }
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
        guard let pending else { return settle() }
        if var chat = shown, chat.key == key {
            let ended = chat.lost(notIn: pending)
            chat.merge(pending, heard: heard)
            shown = chat
            if keepsHistory, !ended.isEmpty { reread(replacing: ended) }
        } else if let index = offPage.firstIndex(where: { $0.key == key }) {
            offPage[index].merge(pending, heard: heard)
        }
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
                shown?.replace(ended, with: page)
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
                shown?.merge(page)
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
