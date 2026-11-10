import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Chats")

/// The chat the palette shows, one at a time. An ask goes on in the daemon while its chat is
/// closed, and shows again when the chat reopens.
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
    /// Why the shown chat's saved asks couldn't be read.
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
    private var rereading: Task<Void, Never>?
    private var listing: Task<Void, Never>?
    /// Events the stream brought during a pending listing, which the listed asks missed.
    private var heard: [Aria_Daemon_V1_ChatEvent?] = []
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    /// Shows the new chat at once. The chat takes its key from the daemon's reply.
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
        show(Chat(key: chat, folder: folder))
        if keepsHistory { read(before: nil) }
        listPending()
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

    /// Stops the shown chat's running ask and every ask waiting behind it. The daemon saves none
    /// of their replies.
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

    /// The event stream replays nothing after it reconnects, so the lost asks get listed again.
    /// One the daemon still runs goes on. One that ended meanwhile is read from history.
    func reconnected() {
        listPending()
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
