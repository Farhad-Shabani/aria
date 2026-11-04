import GRPCCore
import GRPCProtobuf

/// The daemon's ListenService, as far as the menu bar drives it.
enum ListenClient {
    /// Stop answers once the session's final decode has drained.
    static func stop() async throws {
        try await DaemonTransport.call(timeout: .seconds(30)) { client, options in
            _ = try await Aria_Daemon_V1_ListenService.Client(wrapping: client)
                .stop(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    static func isListening() async throws -> Bool {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ListenService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { try $0.message.listening }
        }
    }
}
