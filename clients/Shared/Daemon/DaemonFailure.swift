import GRPCCore
import GRPCProtobuf

/// How a daemon call ended short of an answer.
///
/// The daemon puts its verdict in a `google.rpc.Status` detail holding one
/// `RpcError`; the gRPC code beside it is a lossy projection. A status with
/// that detail is `refused`. One without is transport: nothing listening,
/// a service not registered, a deadline, a cancellation, or anything else.
enum DaemonFailure: Error, Equatable {
    case unavailable
    case unimplemented
    case timeout
    case cancelled
    /// An answer this build cannot read.
    case invalidResponse
    case refused(Aria_Daemon_V1_ErrorCode)
    /// Any other status without a detail. The code is in the transport's log.
    case other

    /// The call never took effect: the daemon answered no, or nothing heard
    /// it. Any other failure leaves the outcome unknown.
    var tookNoEffect: Bool {
        switch self {
        case .refused, .unavailable, .unimplemented: true
        case .timeout, .cancelled, .invalidResponse, .other: false
        }
    }

    private static let detailTypeURL = "type.googleapis.com/aria.daemon.v1.RpcError"

    init(_ error: any Error) {
        guard let rpc = error as? RPCError else {
            self = .other
            return
        }
        if let code = Self.refusal(in: rpc) {
            self = .refused(code)
            return
        }
        switch rpc.code {
        case .unavailable: self = .unavailable
        case .unimplemented: self = .unimplemented
        case .deadlineExceeded: self = .timeout
        case .cancelled: self = .cancelled
        default: self = .other
        }
    }

    private static func refusal(in rpc: RPCError) -> Aria_Daemon_V1_ErrorCode? {
        guard let status = try? rpc.unpackGoogleRPCStatus() else { return nil }
        for detail in status.details {
            guard let packed = detail.any, packed.typeURL == detailTypeURL else { continue }
            return try? Aria_Daemon_V1_RpcError(serializedBytes: packed.value).code
        }
        return nil
    }
}
