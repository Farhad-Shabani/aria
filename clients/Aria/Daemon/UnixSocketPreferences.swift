import GRPCCore
import GRPCNIOTransportHTTP2

/// One short-lived gRPC connection per call to the daemon's PreferencesService
/// over its private Unix socket.
struct UnixSocketPreferences: PreferencesTransport {
    func snapshot() async throws -> Aria_Daemon_V1_PreferenceSnapshot {
        try await withClient { client in
            try await client.get(request: ClientRequest(message: .init())) { try $0.message }
        }
    }

    func update(
        _ request: Aria_Daemon_V1_UpdatePreferencesRequest
    ) async throws -> Aria_Daemon_V1_UpdatePreferencesResponse {
        try await withClient { client in
            try await client.update(request: ClientRequest(message: request)) { try $0.message }
        }
    }

    private func withClient<R: Sendable>(
        _ body:
            @escaping @Sendable (
                Aria_Daemon_V1_PreferencesService.Client<HTTP2ClientTransport.Posix>
            ) async throws -> R
    ) async throws -> R {
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            try await body(Aria_Daemon_V1_PreferencesService.Client(wrapping: client))
        }
    }
}
