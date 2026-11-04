import GRPCCore
import GRPCProtobuf

/// The daemon's health checks. `hotkeyService` is not a gRPC service; the
/// daemon reports its hotkey listener under that name.
enum HealthClient {
    /// Serving once the hotkey listener armed, which needs the
    /// Accessibility grant. Mirrors `HOTKEY_HEALTH_SERVICE` in the daemon.
    static let hotkeyService = "aria.hotkey"

    /// Whether `service` is serving. A daemon that does not answer is not.
    static func isServing(_ service: String) async -> Bool {
        var request = Grpc_Health_V1_HealthCheckRequest()
        request.service = service
        let sent = request
        do {
            return try await DaemonTransport.call { client, options in
                try await Grpc_Health_V1_Health.Client(wrapping: client)
                    .check(request: ClientRequest(message: sent), options: options) {
                        try $0.message.status == .serving
                    }
            }
        } catch {
            return false
        }
    }
}
