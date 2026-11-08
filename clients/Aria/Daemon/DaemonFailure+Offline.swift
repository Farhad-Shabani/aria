extension DaemonFailure {
    static let offlineMessage = "Aria isn’t running. Turn it on in the menu bar, then try again."

    /// A daemon shutting down is about to be unavailable, so both get the same message.
    var isOffline: Bool {
        switch self {
        case .unavailable, .refused(.shuttingDown): true
        case .unimplemented, .timeout, .cancelled, .invalidResponse, .refused, .other: false
        }
    }
}
