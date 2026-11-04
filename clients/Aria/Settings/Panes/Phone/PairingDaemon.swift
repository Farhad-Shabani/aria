import Foundation

/// What phone pairing asks of the daemon and its files. Tests swap it so
/// no socket, config or pairing file is touched.
struct PairingDaemon: Sendable {
    var issueInvite: @Sendable () async throws -> IssuedInvite
    var setPairing: @Sendable (_ enabled: Bool) async throws -> Void
    var removePairingFile: @Sendable () throws -> Void
}

extension PairingDaemon {
    static let live = PairingDaemon(
        issueInvite: { try await InviteClient.issueInvite() },
        setPairing: { enabled in
            try await DaemonProcess.setPhonePairing(enabled)
        },
        removePairingFile: { try PairingFile.remove() }
    )
}
