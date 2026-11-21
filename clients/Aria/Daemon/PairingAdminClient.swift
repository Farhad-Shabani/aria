import Foundation
import GRPCCore
import GRPCProtobuf

struct IssuedInvite: Sendable {
    let invite: PairingInvite
    let expiresAt: Date
    /// Phones paired since the daemon started.
    let pairingCount: UInt64
}

/// A phone your Mac pins.
struct PairedPhone: Equatable, Identifiable, Sendable {
    /// What the daemon pins, and how a revoke names the phone.
    let fingerprint: Data
    /// Such as `iPhone18,1`. Nil when the phone didn't say.
    let hardwareModel: String?
    /// Nil for a phone that paired before your Mac recorded it.
    let pairedAt: Date?

    var id: Data { fingerprint }
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

    static func listPhones() async throws -> [PairedPhone] {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_PairingAdminService.Client(wrapping: client)
                .listPhones(request: ClientRequest(message: .init()), options: options) { response in
                    try response.message.phones.map { phone in
                        PairedPhone(
                            fingerprint: phone.fingerprint,
                            hardwareModel: phone.hardwareModel.isEmpty ? nil : phone.hardwareModel,
                            pairedAt: phone.hasPairTime ? phone.pairTime.date : nil)
                    }
                }
        }
    }

    static func revokePhone(_ fingerprint: Data) async throws {
        var request = Aria_Daemon_V1_RevokePhoneRequest()
        request.fingerprint = fingerprint
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_PairingAdminService.Client(wrapping: client)
                .revokePhone(request: ClientRequest(message: request), options: options) { _ = try $0.message }
        }
    }
}
