import AppKit
import GRPCCore
import GRPCNIOTransportHTTP2
import GRPCProtobuf
import SwiftProtobuf

@MainActor
enum DictationHistoryClient {
    static var effects: DictationHistoryState.Effects {
        .init(list: list, get: get, delete: delete, clear: clear, retry: retry, copy: copy)
    }

    private static func list(_ query: String, _ before: UInt64?) async throws -> HistoryPage {
        var request = Aria_Daemon_V1_ListHistoryRequest()
        request.query = query
        request.limit = 50
        if let before { request.beforeID = before }
        let sent = request
        let response = try await withClient { client in
            try await client.list(request: ClientRequest(message: sent), options: options(seconds: 10)) {
                try $0.message
            }
        }
        return try HistoryPage(
            items: response.entries.map(item),
            next: response.hasNextBeforeID ? response.nextBeforeID : nil,
            warning: response.hasWarning ? response.warning : nil
        )
    }

    private static func get(_ id: UInt64) async throws -> HistoryItem {
        try item(
            await withClient { client in
                try await client.get(
                    request: ClientRequest(message: Aria_Daemon_V1_GetHistoryRequest.with { $0.id = id }),
                    options: options(seconds: 10)
                ) {
                    try $0.message
                }
            })
    }

    private static func delete(_ id: UInt64) async throws {
        _ = try await withClient { client in
            try await client.delete(
                request: ClientRequest(message: Aria_Daemon_V1_DeleteHistoryRequest.with { $0.id = id }),
                options: options(seconds: 10)
            ) {
                try $0.message
            }
        }
    }

    private static func retry(_ id: UInt64) async throws -> HistoryItem {
        try item(
            await withClient { client in
                try await client.regenerate(
                    request: ClientRequest(
                        message: Aria_Daemon_V1_RegenerateHistoryRequest.with { $0.id = id }),
                    options: options(seconds: 670)
                ) {
                    try $0.message
                }
            })
    }

    private static func clear() async throws -> HistoryClearResult {
        do {
            let response = try await withClient { client in
                try await client.clear(
                    request: ClientRequest(message: Aria_Daemon_V1_ClearHistoryRequest()), options: options(seconds: 10)
                ) {
                    try $0.message
                }
            }
            return HistoryClearResult(
                deletedEntries: response.deletedEntryCount, isCleanupPending: response.audioCleanupPending)
        } catch {
            throw clearFailure(error)
        }
    }

    static func clearFailure(_ error: Error) -> HistoryFailure {
        switch error as? HistoryFailure {
        case .busy: .busy
        case .unsupported: .unsupported
        default: .clearUncertain
        }
    }

    private static func copy(_ text: String) -> Bool {
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        guard pasteboard.setString("", forType: .init("org.nspasteboard.ConcealedType")) else {
            return false
        }
        return pasteboard.setString(text, forType: .string)
    }

    static func item(_ wire: Aria_Daemon_V1_HistoryEntry) throws -> HistoryItem {
        guard wire.hasAudioExpiresAtMs == (wire.audioAvailability == .available),
            !wire.hasError || wire.error.code != .unspecified
        else { throw HistoryFailure.failed }
        let audio: HistoryItem.Audio
        switch wire.audioAvailability {
        case .available:
            audio = .available(until: Date(timeIntervalSince1970: Double(wire.audioExpiresAtMs) / 1_000))
        case .unavailable: audio = .unavailable
        case .expired: audio = .expired
        default: throw HistoryFailure.failed
        }
        let status: HistoryItem.Status
        switch wire.status {
        case .success: status = .success
        case .transcriptionFailed: status = .transcriptionFailed
        case .deliveryFailed: status = .deliveryFailed
        case .recovered: status = .recovered
        default: throw HistoryFailure.failed
        }
        let source: String
        switch wire.source {
        case .local: source = "Dictation"
        case .relay: source = "Relayed dictation"
        default: throw HistoryFailure.failed
        }
        return HistoryItem(
            id: wire.id,
            text: wire.text,
            created: Date(timeIntervalSince1970: Double(wire.createdAtMs) / 1_000),
            status: wire.deletionPending ? .deletionPending : status,
            source: source,
            application: wire.hasApplication && !wire.application.name.isEmpty ? wire.application.name : nil,
            duration: wire.hasDurationMs ? Double(wire.durationMs) / 1_000 : nil,
            isComplete: wire.transcriptionComplete,
            isRetryable: wire.canRegenerate,
            audio: audio,
            isRetrying: wire.regenerating,
            warning: wire.hasWarning ? wire.warning : nil,
            message: wire.hasError ? transcriptionMessage(wire.error.code) : nil
        )
    }

    private static func transcriptionMessage(_ code: Aria_Daemon_V1_ErrorCode) -> String {
        switch code {
        case .recordingTimeout, .overallTimeout:
            return "Aria could not finish this recording in time."
        case .captureFailed:
            return "Audio capture failed before transcription finished."
        case .cancelled:
            return "The last retry was cancelled."
        case .historyProjectUnavailable:
            return HistoryFailure.projectUnavailable.message
        case .historyAudioUnavailable:
            return HistoryFailure.audioUnavailable.message
        default:
            return "Aria could not finish transcribing this recording."
        }
    }

    nonisolated private static func options(seconds: Int64) -> CallOptions {
        var options = CallOptions.defaults
        options.timeout = .seconds(seconds)
        return options
    }

    private static func withClient<R: Sendable>(
        _ body:
            @escaping @Sendable (
                Aria_Daemon_V1_HistoryService.Client<HTTP2ClientTransport.Posix>
            ) async throws -> R
    ) async throws -> R {
        do {
            return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
                try await body(Aria_Daemon_V1_HistoryService.Client(wrapping: client))
            }
        } catch {
            throw failure(error)
        }
    }

    static func failure(_ error: Error) -> HistoryFailure {
        if error is CancellationError { return .cancelled }
        guard let rpc = error as? RPCError else { return .offline }
        do {
            if let status = try rpc.unpackGoogleRPCStatus() {
                for detail in status.details {
                    guard let packed = detail.any,
                        packed.typeURL == "type.googleapis.com/aria.daemon.v1.RpcError"
                    else { continue }
                    let error = try Aria_Daemon_V1_RpcError(serializedBytes: packed.value)
                    switch error.code {
                    case .historyUnavailable: return .storage
                    case .historyMissing: return .missing
                    case .historyAudioUnavailable: return .audioUnavailable
                    case .payloadTooLarge: return .tooLarge
                    case .historyProjectUnavailable:
                        return .projectUnavailable
                    default: break
                    }
                }
            }
        } catch {
            return .failed
        }
        switch rpc.code {
        case .unavailable: return .offline
        case .resourceExhausted: return .busy
        case .cancelled: return .cancelled
        case .deadlineExceeded: return .timeout
        case .notFound: return .missing
        case .unimplemented: return .unsupported
        default: return .failed
        }
    }
}
