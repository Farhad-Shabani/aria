import Foundation
import GRPCCore
import GRPCProtobuf

enum SpeakerRole: Equatable, Sendable {
    case owner
    case guest
}

struct EnrolledSpeaker: Equatable, Sendable {
    var name: String
    var role: SpeakerRole

    init(name: String, role: SpeakerRole) {
        self.name = name
        self.role = role
    }

    init(_ wire: Aria_Daemon_V1_EnrolledSpeaker) throws {
        switch wire.role {
        case .owner: self.init(name: wire.name, role: .owner)
        case .guest: self.init(name: wire.name, role: .guest)
        case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
        }
    }
}

struct SpeakerFailure: LocalizedError {
    let errorDescription: String?
}

extension SpeakerFailure {
    /// The daemon's own message is detail for the log; `fallback` names what failed.
    init(_ failure: DaemonFailure, fallback: String) {
        let description: String =
            switch failure {
            case .unavailable: "Aria isn’t running. Turn it on in the menu bar."
            case .refused(.speakerAlreadyEnrolled): "That name is already in use."
            case .refused(.speakerInconsistentSamples):
                "The sentences didn’t sound like one voice. Try again somewhere quiet."
            case .refused(.speakerNoSpeech): "Aria didn’t hear anything. Try again."
            case .refused(.speakerModelMissing):
                "The speaker verification model isn’t downloaded. Open Aria Setup to download it."
            case .refused(.busy): "The microphone is in use. Try again when dictation ends."
            default: fallback
            }
        self.init(errorDescription: description)
    }
}

/// The daemon's SpeakerService. Every failure comes back as a `SpeakerFailure`
/// written for the user; a cancellation passes through untouched.
enum SpeakerClient {
    /// Matches the CLI's guided enrollment.
    static let samples: UInt32 = 5

    static func list() async throws -> [EnrolledSpeaker] {
        try await failing("Aria couldn’t load its voices. Try again.") {
            try await DaemonTransport.call { client, options in
                try await Aria_Daemon_V1_SpeakerService.Client(wrapping: client)
                    .list(request: ClientRequest(message: .init()), options: options) {
                        try $0.message.speakers.map { try EnrolledSpeaker($0) }
                    }
            }
        }
    }

    /// No role lets the daemon choose: a replaced voice keeps its role, and
    /// a new one owns the machine only while nobody does.
    static func enroll(name: String, role: SpeakerRole? = nil, replace: Bool = false) async throws {
        var request = Aria_Daemon_V1_EnrollSpeakerRequest()
        request.name = name
        request.role =
            switch role {
            case .owner: .owner
            case .guest: .guest
            case nil: .unspecified
            }
        request.samples = samples
        request.replace = replace
        let sent = request
        try await failing("Aria couldn’t save the voice. Try again.") {
            try await DaemonTransport.call(timeout: .seconds(180)) { client, options in
                _ = try await Aria_Daemon_V1_SpeakerService.Client(wrapping: client)
                    .enroll(request: ClientRequest(message: sent), options: options) { try $0.message }
            }
        }
    }

    static func remove(name: String) async throws {
        var request = Aria_Daemon_V1_RemoveSpeakerRequest()
        request.name = name
        let sent = request
        try await failing("Aria couldn’t remove the voice. Try again.") {
            try await DaemonTransport.call { client, options in
                _ = try await Aria_Daemon_V1_SpeakerService.Client(wrapping: client)
                    .remove(request: ClientRequest(message: sent), options: options) { try $0.message }
            }
        }
    }

    static func setOwner(name: String) async throws {
        var request = Aria_Daemon_V1_SetOwnerRequest()
        request.name = name
        let sent = request
        try await failing("Aria couldn’t change the owner. Try again.") {
            try await DaemonTransport.call { client, options in
                _ = try await Aria_Daemon_V1_SpeakerService.Client(wrapping: client)
                    .setOwner(request: ClientRequest(message: sent), options: options) { try $0.message }
            }
        }
    }

    private static func failing<R>(_ fallback: String, _ body: () async throws -> R) async throws -> R {
        do {
            return try await body()
        } catch let failure as DaemonFailure {
            throw SpeakerFailure(failure, fallback: fallback)
        }
    }
}
