import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import GRPCProtobuf

/// The daemon's HistoryService. Every failure comes back as a `HistoryFailure`.
@MainActor
enum HistoryClient {
    static var effects: HistoryState.Effects {
        .init(
            list: list, get: get, thread: thread, rename: rename, renameChat: renameChat,
            delete: delete, deleteChat: deleteChat, clear: clear, regenerate: regenerate,
            copy: Pasteboard.copyConcealed)
    }

    private static func list(_ query: String, _ section: HistorySection?, _ before: UInt64?) async throws
        -> HistoryPage
    {
        var request = Aria_Daemon_V1_ListHistoryRequest()
        request.query = query
        if let section {
            request.section =
                switch section {
                case .dictation: .dictation
                case .systemTranscription: .systemTranscription
                case .chat: .chat
                }
        }
        request.limit = 50
        if let before { request.beforeID = before }
        request.collapseChats = true
        let sent = request
        return try await history { client, options in
            try await client.list(request: ClientRequest(message: sent), options: options) {
                let response = try $0.message
                return try HistoryPage(
                    items: response.entries.map { try HistoryItem($0) },
                    next: response.hasNextBeforeID ? response.nextBeforeID : nil,
                    warning: response.hasWarning ? response.warning : nil
                )
            }
        }
    }

    private static func get(_ id: UInt64) async throws -> HistoryItem {
        try await history { client, options in
            try await client.get(
                request: ClientRequest(message: Aria_Daemon_V1_GetHistoryRequest.with { $0.id = id }), options: options
            ) {
                try HistoryItem($0.message)
            }
        }
    }

    private static func thread(_ entry: UInt64) async throws -> [PromptResult] {
        let sent = Aria_Daemon_V1_GetThreadRequest.with { $0.entryID = entry }
        return try await history { client, options in
            try await client.getThread(request: ClientRequest(message: sent), options: options) {
                try $0.message.results.map { result in
                    guard result.hasCreateTime else { throw DaemonFailure.invalidResponse }
                    return PromptResult(
                        id: result.id,
                        created: result.createTime.date,
                        promptName: result.promptName, text: result.text)
                }
            }
        }
    }

    /// The daemon sends no more asks at once (`GetChatRequest.limit`).
    private static let chatPageLimit: UInt32 = 10

    /// `chat`'s newest saved asks before `before`, with their full text.
    static func chat(_ chat: ChatKey, before: UInt64?) async throws -> ChatPage {
        var request = Aria_Daemon_V1_GetChatRequest()
        request.chat = chat.wire
        request.limit = chatPageLimit
        if let before { request.beforeID = before }
        let sent = request
        return try await history { client, options in
            try await client.getChat(request: ClientRequest(message: sent), options: options) {
                let response = try $0.message
                let asks = try response.entries.map { try HistoryItem($0) }
                guard asks.allSatisfy({ $0.chat == chat }) else { throw DaemonFailure.invalidResponse }
                return ChatPage(
                    asks: asks, older: response.hasNextBeforeID ? response.nextBeforeID : nil,
                    quote: response.hasQuote ? response.quote : nil)
            }
        }
    }

    /// Deletes every saved ask of `chat` and its prompt results. A chat already gone succeeds.
    static func deleteChat(_ chat: ChatKey) async throws {
        do {
            try await ChatClient.deleteChat(chat)
        } catch let failure as DaemonFailure {
            throw HistoryFailure(failure)
        } catch is CancellationError {
            throw HistoryFailure.cancelled
        }
    }

    private static func rename(_ id: UInt64, _ title: String?) async throws -> HistoryItem {
        var request = Aria_Daemon_V1_RenameHistoryRequest()
        request.id = id
        if let title { request.title = title }
        let sent = request
        return try await history { client, options in
            try await client.rename(request: ClientRequest(message: sent), options: options) {
                try HistoryItem($0.message.entry)
            }
        }
    }

    private static func renameChat(_ chat: ChatKey, _ name: String) async throws -> ChatKey {
        do {
            return try await ChatClient.renameChat(chat, to: name)
        } catch let failure as DaemonFailure {
            throw HistoryFailure(failure)
        } catch is CancellationError {
            throw HistoryFailure.cancelled
        }
    }

    private static func delete(_ id: UInt64) async throws {
        try await history { client, options in
            _ = try await client.delete(
                request: ClientRequest(message: Aria_Daemon_V1_DeleteHistoryRequest.with { $0.id = id }),
                options: options
            ) {
                try $0.message
            }
        }
    }

    private static func regenerate(_ id: UInt64) async throws -> HistoryItem {
        try await history(timeout: .seconds(670)) { client, options in
            try await client.regenerate(
                request: ClientRequest(message: Aria_Daemon_V1_RegenerateHistoryRequest.with { $0.id = id }),
                options: options
            ) {
                try HistoryItem($0.message)
            }
        }
    }

    private static func clear() async throws -> HistoryClearResult {
        do {
            return try await history { client, options in
                try await client.clear(request: ClientRequest(message: .init()), options: options) {
                    let response = try $0.message
                    return HistoryClearResult(
                        deletedEntries: response.deletedEntryCount, isCleanupPending: response.audioCleanupPending)
                }
            }
        } catch {
            throw clearFailure(error)
        }
    }

    /// A clear that did not answer may still have run, so only a refusal
    /// reads as its own failure.
    static func clearFailure(_ error: Error) -> HistoryFailure {
        switch error as? HistoryFailure {
        case .busy: .busy
        case .unsupported: .unsupported
        default: .clearUncertain
        }
    }

    private static func history<R: Sendable>(
        timeout: Duration = DaemonTransport.defaultTimeout,
        _ body: (Aria_Daemon_V1_HistoryService.Client<HTTP2ClientTransport.Posix>, CallOptions) async throws -> R
    ) async throws -> R {
        do {
            return try await DaemonTransport.call(timeout: timeout) { client, options in
                try await body(Aria_Daemon_V1_HistoryService.Client(wrapping: client), options)
            }
        } catch let failure as DaemonFailure {
            throw HistoryFailure(failure)
        } catch is CancellationError {
            throw HistoryFailure.cancelled
        }
    }
}

extension HistoryItem {
    init(_ wire: Aria_Daemon_V1_HistoryEntry) throws {
        guard wire.hasCreateTime, wire.hasAudioExpireTime == (wire.audioAvailability == .available),
            !wire.hasError || wire.error.code != .unspecified
        else { throw DaemonFailure.invalidResponse }
        let audio: Audio
        switch wire.audioAvailability {
        case .available:
            audio = .available(until: wire.audioExpireTime.date)
        case .unavailable: audio = .unavailable
        case .expired: audio = .expired
        default: throw DaemonFailure.invalidResponse
        }
        let status: Status
        switch wire.status {
        case .success: status = .success
        case .transcriptionFailed: status = .transcriptionFailed
        case .deliveryFailed: status = .deliveryFailed
        case .recovered: status = .recovered
        default: throw DaemonFailure.invalidResponse
        }
        let source: Source
        switch wire.source {
        case .local: source = .local
        case .relay: source = .relay
        default: throw DaemonFailure.invalidResponse
        }
        let kind = try Self.kind(wire)
        let chat = wire.hasChat ? try ChatKey(wire.chat) : nil
        if chat != nil {
            guard case .ask = kind else { throw DaemonFailure.invalidResponse }
        }
        self.init(
            id: wire.id,
            title: wire.hasTitle ? wire.title : nil,
            kind: kind,
            created: wire.createTime.date,
            duration: wire.hasDuration ? wire.duration.timeInterval : nil,
            source: source,
            application: wire.hasApplication && !wire.application.name.isEmpty ? wire.application.name : nil,
            project: wire.hasProjectDirectory ? wire.projectDirectory : nil,
            chat: chat,
            text: wire.text,
            isComplete: wire.transcriptionComplete,
            resultCount: Int(wire.resultCount),
            status: wire.deletionPending ? .deletionPending : status,
            message: wire.hasError ? Self.transcriptionMessage(wire.error.code) : nil,
            warning: wire.hasWarning ? wire.warning : nil,
            audio: audio,
            isRegenerating: wire.regenerating,
            canRegenerate: wire.canRegenerate
        )
    }

    private static func kind(_ wire: Aria_Daemon_V1_HistoryEntry) throws -> Kind {
        switch (wire.section, wire.details) {
        // A daemon from before sections sends none, and it kept only dictations.
        case (.dictation, nil), (.unspecified, nil):
            return .dictation
        case (.systemTranscription, .transcript(let transcript)?):
            let read: TranscriptRead?
            switch transcript.hasRead ? transcript.read : nil {
            case nil: read = nil
            case .whole?: read = .whole
            case .truncated?: read = .truncated
            case .unavailable?: read = .unavailable
            default: throw DaemonFailure.invalidResponse
            }
            return .systemTranscription(transcript: transcript.path, read: read)
        case (.chat, .reply(let reply)?):
            return .ask(reply: reply)
        default:
            throw DaemonFailure.invalidResponse
        }
    }

    private static func transcriptionMessage(_ code: Aria_Daemon_V1_ErrorCode) -> String {
        switch code {
        case .recordingTimeout, .overallTimeout:
            return "Aria couldn’t finish this recording in time."
        case .captureFailed:
            return "Aria couldn’t record audio, so transcription didn’t finish."
        case .cancelled:
            return "Transcribing again was canceled."
        case .historyProjectUnavailable:
            return HistoryFailure.projectUnavailable.message
        case .historyAudioUnavailable:
            return HistoryFailure.audioUnavailable.message
        default:
            return "Aria couldn’t finish transcribing this recording."
        }
    }
}
