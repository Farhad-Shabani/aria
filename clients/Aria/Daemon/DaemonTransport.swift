import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonTransport")

/// One short-lived connection over the daemon's Unix socket per call.
enum DaemonTransport {
    /// Covers any call the daemon answers at once. A call that records or
    /// runs a model names its own.
    static let defaultTimeout = Duration.seconds(10)

    /// Runs `body` on its own connection, with `options` carrying the
    /// deadline. A cancellation or a `DaemonFailure` raised inside passes
    /// through; any other error is logged here, once, and decoded.
    static func call<R: Sendable>(
        timeout: Duration? = defaultTimeout,
        socket: URL = RuntimeFiles.daemonSocketURL,
        isolation: isolated (any Actor)? = #isolation,
        _ body: (GRPCClient<HTTP2ClientTransport.Posix>, CallOptions) async throws -> R
    ) async throws -> R {
        var options = CallOptions.defaults
        options.timeout = timeout
        do {
            return try await withGRPCClient(transport: unixSocket(at: socket), isolation: isolation) { client in
                try await body(client, options)
            }
        } catch let error where error is CancellationError || error is DaemonFailure {
            throw error
        } catch {
            let failure = DaemonFailure(error)
            // A daemon that is not running is a state the app shows, not a fault.
            if failure == .unavailable {
                log.debug("Daemon call found nothing listening: \(String(describing: error), privacy: .private)")
            } else {
                log.error("Daemon call failed: \(String(describing: error), privacy: .private)")
            }
            throw failure
        }
    }

    /// A socket path is not a valid HTTP/2 :authority, and the daemon resets
    /// the stream when it sees one.
    private static func unixSocket(at socket: URL) throws -> HTTP2ClientTransport.Posix {
        try .http2NIOPosix(
            target: .unixDomainSocket(path: socket.path, authority: "localhost"),
            transportSecurity: .plaintext
        )
    }
}
