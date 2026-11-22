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

    /// The daemon sends an empty path for no file.
    static func transcriptFile(atPath path: String) -> URL? {
        path.isEmpty ? nil : URL(filePath: path)
    }
}
