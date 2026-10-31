import GRPCCore

extension DaemonSettings {
    /// Nil while the daemon is not listening, which the wait message covers.
    var loadFailure: String? {
        guard let lastError, (lastError as? RPCError)?.code != .unavailable else { return nil }
        return "Aria couldn’t load its settings. Run `aria doctor` in Terminal to see why."
    }

    /// On the Mac's own socket, the daemon refuses a write only when the file
    /// is read-only. A phone's refusals never reach this class on the Mac.
    var isReadOnlyFailure: Bool {
        (lastError as? RPCError)?.code == .permissionDenied
    }
}
