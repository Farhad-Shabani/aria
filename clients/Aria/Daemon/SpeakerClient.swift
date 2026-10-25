import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import GRPCProtobuf

struct SpeakerFailure: LocalizedError {
    let errorDescription: String?
}

/// One short-lived gRPC connection per call to the daemon's SpeakerService.
enum SpeakerClient {
    /// Matches the CLI's guided enrollment.
    static let samples: UInt32 = 5

    static func list() async throws -> [Aria_Daemon_V1_EnrolledSpeaker] {
        try await withClient { client in
            try await client.list(request: ClientRequest(message: .init())) { try $0.message.speakers }
        }
    }

    /// An unspecified role lets the daemon choose: owner for the machine's
    /// first voice, guest for every one after. That rule is what setup wants.
    static func enroll(
        name: String,
        role: Aria_Daemon_V1_SpeakerRole = .unspecified,
        replace: Bool = false
    ) async throws -> Aria_Daemon_V1_EnrollSpeakerResponse {
        try await withClient { client in
            var request = Aria_Daemon_V1_EnrollSpeakerRequest()
            request.name = name
            request.role = role
            request.samples = samples
            request.replace = replace
            var options = CallOptions.defaults
            options.timeout = .seconds(180)
            return try await client.enroll(request: ClientRequest(message: request), options: options) {
                try $0.message
            }
        }
    }

    static func remove(name: String) async throws {
        try await withClient { client in
            var request = Aria_Daemon_V1_RemoveSpeakerRequest()
            request.name = name
            _ = try await client.remove(request: ClientRequest(message: request)) { try $0.message }
        }
    }

    private static func withClient<R: Sendable>(
        _ body:
            @escaping @Sendable (
                Aria_Daemon_V1_SpeakerService.Client<HTTP2ClientTransport.Posix>
            ) async throws -> R
    ) async throws -> R {
        do {
            return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
                try await body(Aria_Daemon_V1_SpeakerService.Client(wrapping: client))
            }
        } catch let rpc as RPCError {
            throw failure(rpc)
        }
    }

    /// Cancellation passes through untouched so callers can tell it apart.
    private static func failure(_ rpc: RPCError) -> any Error {
        switch rpc.code {
        case .cancelled: return rpc
        case .unavailable: return SpeakerFailure(errorDescription: "Aria isn’t running.")
        default: break
        }
        let description: String =
            switch daemonCode(rpc) {
            case .speakerAlreadyEnrolled: "That name is already in use."
            case .speakerInconsistentSamples: "The sentences didn’t sound like one voice. Try again somewhere quiet."
            case .speakerNoSpeech: "Aria didn’t hear anything. Try again."
            case .speakerModelMissing: "The speaker verification model isn’t downloaded."
            case .busy: "The microphone is in use. Try again when dictation ends."
            default: rpc.message.prefix(1).uppercased() + rpc.message.dropFirst()
            }
        return SpeakerFailure(errorDescription: description)
    }

    /// Nil when the details are unreadable; the status message still says what failed.
    private static func daemonCode(_ rpc: RPCError) -> Aria_Daemon_V1_ErrorCode? {
        guard let status = try? rpc.unpackGoogleRPCStatus() else { return nil }
        for detail in status.details {
            guard let packed = detail.any, packed.typeURL == "type.googleapis.com/aria.daemon.v1.RpcError" else {
                continue
            }
            return try? Aria_Daemon_V1_RpcError(serializedBytes: packed.value).code
        }
        return nil
    }
}
