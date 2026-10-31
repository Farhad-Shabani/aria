/// The two calls `DaemonSettings` makes, behind whatever carries them:
/// the Mac's Unix socket, a phone's paired TLS connection, or a test fake.
@MainActor
protocol SettingsTransport {
    func snapshot() async throws -> Aria_Daemon_V1_SettingsSnapshot
    func update(
        _ request: Aria_Daemon_V1_UpdateSettingsRequest
    ) async throws -> Aria_Daemon_V1_UpdateSettingsResponse
}
