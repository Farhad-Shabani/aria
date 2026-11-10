import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Chats")

/// The chat the palette shows: its saved asks, the ones the daemon still runs, the ones
/// asked since, and their replies as the event stream brings them. One shows at a time. An ask goes
/// on in the daemon while its chat is closed, and shows again when it reopens.
@MainActor
final class Chats: ObservableObject {
    struct Effects {
        /// Sends the first question of a chat the daemon names, typing nothing.
        var start: (_ text: String, _ target: ChatTarget, _ ownVoice: Bool) async throws -> StartedChat
        /// Sends a question in a chat, typing nothing, and returns its ticket. The daemon
        /// runs it in the folder it recorded for the chat, or in `folder` when it has none.
        var ask:
            (
                _ text: String, _ chat: ChatKey, _ folder: String?, _ ownVoice: Bool,
                _ tapCancels: Bool
            ) async throws -> UInt64
        /// Stops a chat's running ask and every one waiting behind it.
        var cancel: (ChatKey) async throws -> Void
        /// A chat's newest saved asks before a cursor.
        var read: (ChatKey, _ before: UInt64?) async throws -> ChatPage
        /// A chat's asks the daemon runs or has waiting, in the order it runs them.
        var pending: (ChatKey) async throws -> [Aria_Daemon_V1_PendingAsk]
    }

    @Published private(set) var shown: Chat?
    /// The chat a spoken follow-up is being recorded for.
    @Published var recording: ChatKey?
    /// Why its saved asks couldn't be read.
    @Published private(set) var error: HistoryFailure?
    /// Why the last Stop didn't reach the daemon, while the ask it meant to stop still runs.
    @Published private(set) var stopFailure: String?
    /// While off, nothing is read from history: a chat holds what was asked here and the
    /// asks the daemon still runs.
    var keepsHistory = false
    /// The cursor of the last read, which Try Again reads again. `nil` reads the newest asks.
    private var lastRead: UInt64?
    /// Numbers the asks asked here, across chats, so a late ticket finds only its own.
    private var asked = 0
    private var reading: Task<Void, Never>?
    /// The read that swaps the asks the stream lost, which ended while it was gone, for what
    /// history saved.
    private var rereading: Task<Void, Never>?
    private var listing: Task<Void, Never>?
    /// What the stream brought while the pending asks were listed, which the asks listed missed.
    private var heard: [Aria_Daemon_V1_ChatEvent?] = []
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    /// Starts a chat with the assistant and folder `target` names. It shows at once, and
    /// takes its key from the daemon's reply.
    func startNew(_ question: String, at target: ChatTarget, ownVoice: Bool) {
        show(Chat(key: nil, folder: target.workdir))
        let id = append(question)
        Task {
            do {
                let started = try await effects.start(question, target, ownVoice)
                shown?.named(started.chat, by: id)
                shown?.started(id, ticket: started.ticket)
            } catch {
                fail(id, error)
            }
        }
    }

    /// A spoken follow-up. The chat may have closed while it was recorded, so it opens
    /// again, naming `folder`.
    func followUp(_ question: String, in chat: ChatKey, folder: String?, ownVoice: Bool) {
        open(chat, folder: folder)
        ask(question, ownVoice: ownVoice, tapCancels: true)
    }

    /// Whether the chat that shows takes a typed follow-up: not while its last ask replies,
    /// nor while a spoken one is recorded for it.
    var takesFollowUp: Bool {
        guard let shown, let key = shown.key else { return false }
        return !shown.isReplying && recording != key
    }

    /// Whether `text` would go as a typed follow-up: something besides spaces, while one is taken.
    func canSend(_ text: String) -> Bool {
        takesFollowUp && !Self.question(text).isEmpty
    }

    /// A typed follow-up in the chat that shows, while it takes one. Returns whether it
    /// was sent.
    func send(_ text: String) -> Bool {
        guard canSend(text) else { return false }
        ask(Self.question(text), ownVoice: false, tapCancels: false)
        return true
    }

    private static func question(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Shows `chat`, with its saved asks while history is on, then the asks the daemon
    /// still runs. `folder` is where its asks last ran, for the page to name. The one already
    /// showing stays as it is.
    func open(_ chat: ChatKey, folder: String?) {
        guard shown?.key != chat else { return }
        show(Chat(key: chat, folder: folder))
        if keepsHistory { read(before: nil) }
        listPending()
    }

    /// Reads the saved asks before the oldest one shown.
    func readEarlier() {
        guard reading == nil, error == nil, let older = shown?.older else { return }
        read(before: older)
    }

    /// Reads again the asks a read couldn't get.
    func retryRead() {
        guard reading == nil, error != nil, shown != nil else { return }
        error = nil
        read(before: lastRead)
    }

    /// Stops the chat that shows: the ask the assistant runs, and every one waiting behind
    /// it. Their replies aren't saved.
    func stop() {
        guard let shown, shown.canStop, let key = shown.key else { return }
        stopFailure = nil
        Task {
            do {
                try await effects.cancel(key)
            } catch {
                log.error("Stopping a chat failed: \(String(describing: error), privacy: .private)")
                guard self.shown?.key == key, self.shown?.isReplying == true else { return }
                stopFailure =
                    (error as? DaemonFailure)?.isOffline == true
                    ? DaemonFailure.offlineMessage : Self.stopFailed
            }
        }
    }

    func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        if listing != nil { heard.append(event) }
        shown?.apply(event)
        if shown?.isReplying != true { stopFailure = nil }
    }

    /// The event stream connected again. It replays nothing, so the asks it lost are listed
    /// again: one the daemon still runs goes on, and one that ended meanwhile is read from history.
    func reconnected() {
        listPending()
    }

    /// `chat` goes by `renamed` from now on.
    func renamed(_ chat: ChatKey, to renamed: ChatKey) {
        if shown?.key == chat { shown?.rename(to: renamed) }
        if recording == chat { recording = renamed }
    }

    /// A prompt result was saved under `entry`, so the ask saved as it counts one more.
    func countResult(on entry: UInt64) {
        shown?.countResult(on: entry)
    }

    /// Whether the pill should leave this ticket's events to the palette.
    func shows(ticket: UInt64) -> Bool {
        shown?.asks.contains { $0.ticket == ticket } ?? false
    }

    func close() {
        for task in [reading, rereading, listing] { task?.cancel() }
        reading = nil
        rereading = nil
        listing = nil
        heard = []
        shown = nil
        error = nil
        stopFailure = nil
    }

    private func show(_ chat: Chat) {
        close()
        shown = chat
    }

    private func ask(_ question: String, ownVoice: Bool, tapCancels: Bool) {
        guard let key = shown?.key else { return }
        let folder = shown?.folder
        let id = append(question)
        Task {
            do {
                let ticket = try await effects.ask(question, key, folder, ownVoice, tapCancels)
                shown?.started(id, ticket: ticket)
            } catch {
                fail(id, error)
            }
        }
    }

    /// Shows `question` as asked, before its ticket comes.
    private func append(_ question: String) -> ChatAsk.ID {
        asked += 1
        shown?.append(ChatAsk(live: asked, question: question))
        return .live(asked)
    }

    private func fail(_ id: ChatAsk.ID, _ error: Error) {
        shown?.fail(id, Self.message(for: error))
    }

    private func listPending() {
        guard let key = shown?.key else { return }
        listing?.cancel()
        heard = []
        listing = Task {
            defer {
                if !Task.isCancelled {
                    listing = nil
                    heard = []
                }
            }
            do {
                let pending = try await effects.pending(key)
                guard !Task.isCancelled, shown?.key == key else { return }
                let ended = shown?.lost(notIn: pending) ?? []
                shown?.merge(pending, heard: heard)
                if keepsHistory, !ended.isEmpty { reread(replacing: ended) }
            } catch {
                guard !Task.isCancelled else { return }
                log.error("Listing a chat's asks failed: \(String(describing: error), privacy: .private)")
            }
        }
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
            start: { try await ChatClient.start(text: $0, at: $1, ownVoice: $2) },
            ask: { try await ChatClient.ask(text: $0, in: $1, folder: $2, ownVoice: $3, tapCancels: $4) },
            cancel: { try await ChatClient.cancel(chat: $0) },
            read: { try await HistoryClient.chat($0, before: $1) },
            pending: { try await ChatClient.pendingAsks(in: $0) })
    }
}
