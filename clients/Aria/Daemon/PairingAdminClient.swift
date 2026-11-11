import Foundation
import GRPCCore
import GRPCProtobuf

struct IssuedInvite: Sendable {
    let invite: PairingInvite
    let expiresAt: Date
    /// Phones paired since the daemon started.
    let pairingCount: UInt64
}

/// The daemon's PairingAdminService, registered only while the relay runs.
enum PairingAdminClient {
    static func issueInvite() async throws -> IssuedInvite {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_PairingAdminService.Client(wrapping: client)
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

    static func revokePhones() async throws {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_PairingAdminService.Client(wrapping: client)
                .revokePhones(request: ClientRequest(message: .init()), options: options) { _ = try $0.message }
        }
    }
}
