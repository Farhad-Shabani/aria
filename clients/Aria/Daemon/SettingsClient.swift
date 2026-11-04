import GRPCCore
import GRPCProtobuf

/// The daemon's SettingsService over the Mac's own socket.
struct SettingsClient: SettingsTransport {
    func snapshot() async throws -> Aria_Daemon_V1_SettingsSnapshot {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SettingsService.Client(wrapping: client)
                .get(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    func update(
        _ request: Aria_Daemon_V1_UpdateSettingsRequest
    ) async throws -> Aria_Daemon_V1_UpdateSettingsResponse {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SettingsService.Client(wrapping: client)
                .update(request: ClientRequest(message: request), options: options) { try $0.message }
        }
    }

    /// Asks whether the daemon would accept the patch. Writes nothing.
    func check(_ request: Aria_Daemon_V1_CheckSettingsRequest) async throws {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SettingsService.Client(wrapping: client)
                .check(request: ClientRequest(message: request), options: options) { _ in }
        }
    }

    func status() async throws -> Aria_Daemon_V1_SettingsStatus {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SettingsService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { response in
                    let message = try response.message
                    guard message.hasStatus else { throw DaemonFailure.invalidResponse }
                    return message.status
                }
        }
    }
}
