import GRPCCore

extension DaemonPreferences {
    /// Nil while the daemon is not listening, which the wait message covers.
    var loadFailure: String? {
        guard let lastError, (lastError as? RPCError)?.code != .unavailable else { return nil }
        return "Aria couldn’t load its settings. Run `aria doctor` in Terminal to see why."
    }
}
