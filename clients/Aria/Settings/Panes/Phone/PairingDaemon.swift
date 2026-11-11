import Foundation

/// What phone pairing asks of the daemon. Tests swap it so no socket or
/// settings file is touched.
struct PairingDaemon: Sendable {
    var issueInvite: @Sendable () async throws -> IssuedInvite
    var setPairing: @Sendable (_ enabled: Bool) async throws -> Void
    var revokePhones: @Sendable () async throws -> Void
}

extension PairingDaemon {
    static let live = PairingDaemon(
        issueInvite: { try await PairingAdminClient.issueInvite() },
        setPairing: { enabled in
            try await DaemonProcess.setPhonePairing(enabled)
        },
        revokePhones: { try await PairingAdminClient.revokePhones() }
    )
}
