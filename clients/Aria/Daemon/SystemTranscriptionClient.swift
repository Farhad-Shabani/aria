import GRPCCore
import GRPCProtobuf

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

    static func isRunning() async throws -> Bool {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { try $0.message.running }
        }
    }
}
