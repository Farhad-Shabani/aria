import GRPCCore
import GRPCNIOTransportHTTP2

/// One short-lived gRPC connection per call to the daemon's SettingsService
/// over its private Unix socket.
struct UnixSocketSettings: SettingsTransport {
    func snapshot() async throws -> Aria_Daemon_V1_SettingsSnapshot {
        try await withClient { client in
            try await client.get(request: ClientRequest(message: .init())) { try $0.message }
        }
    }

    func update(
        _ request: Aria_Daemon_V1_UpdateSettingsRequest
    ) async throws -> Aria_Daemon_V1_UpdateSettingsResponse {
        try await withClient { client in
            try await client.update(request: ClientRequest(message: request)) { try $0.message }
        }
    }

    func status() async throws -> Aria_Daemon_V1_SettingsStatus {
        try await withClient { client in
            try await client.getStatus(request: ClientRequest(message: .init())) { response in
                let message = try response.message
                guard message.hasStatus else {
                    throw DaemonClientError.commandFailed("the daemon sent no settings status")
                }
                return message.status
            }
        }
    }

    private func withClient<R: Sendable>(
        _ body:
            @escaping @Sendable (
                Aria_Daemon_V1_SettingsService.Client<HTTP2ClientTransport.Posix>
            ) async throws -> R
    ) async throws -> R {
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            try await body(Aria_Daemon_V1_SettingsService.Client(wrapping: client))
        }
    }
}
