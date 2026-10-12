/// The two calls `DaemonPreferences` makes, behind whatever carries them:
/// the Mac's Unix socket, a phone's paired TLS connection, or a test fake.
@MainActor
protocol PreferencesTransport {
    func snapshot() async throws -> Aria_Daemon_V1_PreferenceSnapshot
    func update(
        _ request: Aria_Daemon_V1_UpdatePreferencesRequest
    ) async throws -> Aria_Daemon_V1_UpdatePreferencesResponse
}
