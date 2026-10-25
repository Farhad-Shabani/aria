enum HistoryFailure: Error, Equatable {
    case offline, storage, missing, audioUnavailable, projectUnavailable, tooLarge, busy, cancelled,
        timeout, queryTooLong, unsupported, clearUncertain, failed

    var message: String {
        switch self {
        case .offline: "Aria is unavailable. Start the daemon and try again."
        case .storage: "Dictation history storage is unavailable."
        case .missing: "This dictation is no longer available. Refresh history."
        case .audioUnavailable: "The recording is no longer available for retry."
        case .projectUnavailable: "The original project directory is unavailable. Restore it before retrying."
        case .tooLarge: "The transcript is too large for history to store."
        case .busy: "Aria is busy. Try again when the current operation finishes."
        case .cancelled: "Retry was cancelled."
        case .timeout: "The operation took too long. Try again."
        case .queryTooLong: "Use a shorter search (up to 256 bytes)."
        case .unsupported: "Update and restart Aria to clear dictation history."
        case .clearUncertain: "Aria could not confirm whether history was cleared. Refresh history, then try again."
        case .failed: "The operation failed. Please try again."
        }
    }
}
