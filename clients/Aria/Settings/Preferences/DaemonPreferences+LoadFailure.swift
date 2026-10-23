import GRPCCore

extension DaemonPreferences {
    /// Nil while the daemon is not listening, which the wait message covers.
    /// Any other error means it answered and refused.
    var loadFailure: String? {
        guard let lastError, (lastError as? RPCError)?.code != .unavailable else { return nil }
        return lastError.localizedDescription
    }
}
