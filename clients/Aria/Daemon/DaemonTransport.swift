import GRPCCore
import GRPCNIOTransportHTTP2

enum DaemonTransport {
    /// A socket path is not a valid HTTP/2 :authority, and the daemon resets
    /// the stream when it sees one.
    static func unixSocket() throws -> HTTP2ClientTransport.Posix {
        try .http2NIOPosix(
            target: .unixDomainSocket(
                path: RuntimeFiles.daemonSocketURL.path, authority: "localhost"
            ),
            transportSecurity: .plaintext
        )
    }

    static func isCancellation(_ error: any Error) -> Bool {
        error is CancellationError || (error as? RPCError)?.code == .cancelled
    }
}
