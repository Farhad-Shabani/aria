import GRPCCore
import GRPCNIOTransportHTTP2

/// The daemon's health checks. `hotkeyService` is not a gRPC service; the
/// daemon reports its hotkey listener under that name.
enum HealthClient {
    /// Serving once the hotkey listener armed, which needs the
    /// Accessibility grant. Mirrors `HOTKEY_HEALTH_SERVICE` in the daemon.
    static let hotkeyService = "aria.hotkey"

    /// Whether `service` is serving. A daemon that does not answer is not.
    static func serving(_ service: String) async -> Bool {
        var request = Grpc_Health_V1_HealthCheckRequest()
        request.service = service
        do {
            return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
                let health = Grpc_Health_V1_Health.Client(wrapping: client)
                return try await health.check(request: ClientRequest(message: request)) {
                    try $0.message.status == .serving
                }
            }
        } catch {
            return false
        }
    }
}
