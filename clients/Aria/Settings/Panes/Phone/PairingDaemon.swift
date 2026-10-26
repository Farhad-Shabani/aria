import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2

/// What phone pairing asks of the daemon and its files. Tests swap it so
/// no socket, config or pairing file is touched.
struct PairingDaemon: Sendable {
    var issueInvite: @Sendable () async throws -> (invite: PairingInvite, expiresAt: Date)
    var setPairing: @Sendable (_ enabled: Bool) async throws -> Void
    var removePairingFile: @Sendable () throws -> Void
}

extension PairingDaemon {
    static let live = PairingDaemon(
        issueInvite: {
            try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
                let invites = Aria_Daemon_V1_InviteService.Client(wrapping: client)
                return try await invites.issueInvite(Aria_Daemon_V1_IssueInviteRequest()) { response in
                    let issued = try response.message
                    let invite = PairingInvite(
                        instanceName: issued.instanceName, code: issued.code,
                        fingerprint: issued.fingerprint)
                    return (invite, Date.now.addingTimeInterval(TimeInterval(issued.expiresInMs) / 1000))
                }
            }
        },
        setPairing: { enabled in
            try await DaemonClient.setPhonePairing(enabled)
        },
        removePairingFile: { try PairingFile.remove() }
    )
}
