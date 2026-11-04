import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import GRPCProtobuf

/// The daemon's HistoryService. Every failure comes back as a `HistoryFailure`.
@MainActor
enum HistoryClient {
    static var effects: DictationHistoryState.Effects {
        .init(
            list: list, get: get, delete: delete, clear: clear, regenerate: regenerate, copy: Pasteboard.copyConcealed)
    }

    private static func list(_ query: String, _ before: UInt64?) async throws -> HistoryPage {
        var request = Aria_Daemon_V1_ListHistoryRequest()
        request.query = query
        request.limit = 50
        if let before { request.beforeID = before }
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
        } catch {
            // Only a cancellation leaves `call` undecoded.
            throw HistoryFailure.cancelled
        }
    }
}

extension HistoryItem {
    init(_ wire: Aria_Daemon_V1_HistoryEntry) throws {
        guard wire.hasAudioExpiresAtMs == (wire.audioAvailability == .available),
            !wire.hasError || wire.error.code != .unspecified
        else { throw DaemonFailure.invalidResponse }
        let audio: Audio
        switch wire.audioAvailability {
        case .available:
            audio = .available(until: Date(timeIntervalSince1970: Double(wire.audioExpiresAtMs) / 1_000))
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
        self.init(
            id: wire.id,
            text: wire.text,
            created: Date(timeIntervalSince1970: Double(wire.createdAtMs) / 1_000),
            status: wire.deletionPending ? .deletionPending : status,
            source: source,
            application: wire.hasApplication && !wire.application.name.isEmpty ? wire.application.name : nil,
            duration: wire.hasDurationMs ? Double(wire.durationMs) / 1_000 : nil,
            isComplete: wire.transcriptionComplete,
            canRegenerate: wire.canRegenerate,
            audio: audio,
            isRegenerating: wire.regenerating,
            warning: wire.hasWarning ? wire.warning : nil,
            message: wire.hasError ? Self.transcriptionMessage(wire.error.code) : nil
        )
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
