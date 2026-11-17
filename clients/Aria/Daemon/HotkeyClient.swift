import GRPCCore
import GRPCProtobuf

enum AriaKeyState: Equatable {
    case armed
    case needsAccessibility
    case needsInputMonitoring
    /// No permission explains why macOS refused the key.
    case refused
}

enum HotkeyClient {
    /// Only the daemon can tell, since macOS grants each process its own
    /// permissions.
    static func state() async -> AriaKeyState {
        let reported = try? await reportedState()
        return await resolve(reported, isServing: { await HealthClient.isServing(HealthClient.hotkeyService) })
    }

    /// An older daemon reports no state, and a starting one doesn't know it
    /// yet. Both fall back to health, which only says whether the key is armed.
    static func resolve(
        _ reported: Aria_Daemon_V1_HotkeyState?, isServing: () async -> Bool
    ) async -> AriaKeyState {
        switch reported {
        case .armed: .armed
        case .awaitingPermission: .needsAccessibility
        case .awaitingListenAccess: .needsInputMonitoring
        case .refused: .refused
        case .keyUnavailable, .unspecified, .UNRECOGNIZED, nil:
            await isServing() ? .armed : .needsAccessibility
        }
    }

    private static func reportedState() async throws -> Aria_Daemon_V1_HotkeyState {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_HotkeyService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { try $0.message.state }
        }
    }
}
