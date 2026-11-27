import Foundation
import GRPCCore
import GRPCProtobuf

struct SystemTranscriptionSnapshot: Sendable, Equatable {
    let isRunning: Bool
    /// The running session's file, or the last one's. `nil` before any session.
    let transcriptFile: URL?
}

enum SystemTranscriptionClient {
    static func start() async throws {
        try await DaemonTransport.call { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .start(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    /// Stop answers once the session's final decode has drained.
    static func stop() async throws {
        try await DaemonTransport.call(timeout: .seconds(30)) { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .stop(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    static func status() async throws -> SystemTranscriptionSnapshot {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { response in
                    let message = try response.message
                    return SystemTranscriptionSnapshot(
                        isRunning: message.running, transcriptFile: transcriptFile(atPath: message.transcriptPath))
                }
        }
    }

    static func listUnsaved() async throws -> UnsavedTranscriptList {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .listUnsaved(request: ClientRequest(message: .init()), options: options) { response in
                    let message = try response.message
                    guard !message.transcriptsDir.isEmpty, !message.unsavedDir.isEmpty else {
                        throw DaemonFailure.invalidResponse
                    }
                    return UnsavedTranscriptList(
                        transcripts: try message.transcripts.map(UnsavedTranscript.init),
                        isHistoryFull: message.historyFull,
                        transcriptsDir: URL(filePath: message.transcriptsDir, directoryHint: .isDirectory),
                        unsavedDir: URL(filePath: message.unsavedDir, directoryHint: .isDirectory))
                }
        }
    }

    static func readUnsaved(_ transcript: UnsavedTranscript) async throws -> UnsavedTranscriptText {
        let sent = Aria_Daemon_V1_ReadUnsavedTranscriptRequest.with {
            $0.name = transcript.name
        }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .readUnsaved(request: ClientRequest(message: sent), options: options) { response in
                    let message = try response.message
                    switch message.read {
                    case .whole: return .whole(message.text)
                    case .truncated: return .truncated(message.text)
                    case .unspecified, .unavailable, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
                    }
                }
        }
    }

    static func discardUnsaved(_ transcript: UnsavedTranscript) async throws {
        let sent = Aria_Daemon_V1_DiscardUnsavedTranscriptRequest.with {
            $0.name = transcript.name
        }
        try await DaemonTransport.call { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .discardUnsaved(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }

    /// The daemon sends an empty path for no file.
    static func transcriptFile(atPath path: String) -> URL? {
        path.isEmpty ? nil : URL(filePath: path)
    }
}

extension UnsavedTranscript {
    init(_ wire: Aria_Daemon_V1_UnsavedTranscript) throws {
        guard !wire.name.isEmpty, !wire.path.isEmpty, wire.hasStartedAt else {
            throw DaemonFailure.invalidResponse
        }
        self.init(
            name: wire.name, path: URL(filePath: wire.path), startedAt: wire.startedAt.date,
            duration: wire.duration.timeInterval)
    }
}
