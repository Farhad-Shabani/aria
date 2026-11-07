import Foundation
import GRPCCore
import GRPCProtobuf

struct IssuedInvite: Sendable {
    let invite: PairingInvite
    let expiresAt: Date
    /// Phones paired since the daemon started.
    let pairingCount: UInt64
}

/// The daemon's InviteService, registered only while the relay runs.
enum InviteClient {
    static func issueInvite() async throws -> IssuedInvite {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_InviteService.Client(wrapping: client)
                .issueInvite(request: ClientRequest(message: .init()), options: options) { response in
                    let issued = try response.message
                    return IssuedInvite(
                        invite: PairingInvite(
                            instanceName: issued.instanceName, code: issued.code, fingerprint: issued.fingerprint),
                        expiresAt: Date.now.addingTimeInterval(issued.expiresIn.timeInterval),
                        pairingCount: issued.pairingCount)
                }
        }
    }
}
