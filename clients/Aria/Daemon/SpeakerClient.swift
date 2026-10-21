import GRPCCore
import GRPCNIOTransportHTTP2

/// One short-lived gRPC connection per call to the daemon's SpeakerService.
enum SpeakerClient {
    /// Matches the CLI's guided enrollment.
    private static let samples: UInt32 = 5

    static func list() async throws -> [Aria_Daemon_V1_EnrolledSpeaker] {
        try await withClient { client in
            try await client.list(request: ClientRequest(message: .init())) { try $0.message.speakers }
        }
    }

    /// The daemon gives the first voice on a machine owner, and every one
    /// after guest; that rule is what setup wants, so no role is sent.
    static func enroll(name: String) async throws -> Aria_Daemon_V1_EnrollSpeakerResponse {
        try await withClient { client in
            var request = Aria_Daemon_V1_EnrollSpeakerRequest()
            request.name = name
            request.samples = samples
            var options = CallOptions.defaults
            options.timeout = .seconds(180)
            return try await client.enroll(request: ClientRequest(message: request), options: options) {
                try $0.message
            }
        }
    }

    private static func withClient<R: Sendable>(
        _ body:
            @escaping @Sendable (
                Aria_Daemon_V1_SpeakerService.Client<HTTP2ClientTransport.Posix>
            ) async throws -> R
    ) async throws -> R {
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            try await body(Aria_Daemon_V1_SpeakerService.Client(wrapping: client))
        }
    }
}
