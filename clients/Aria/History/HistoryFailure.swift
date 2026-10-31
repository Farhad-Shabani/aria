enum HistoryFailure: Error, Equatable {
    case offline, storage, missing, audioUnavailable, projectUnavailable, tooLarge, busy, cancelled,
        timeout, queryTooLong, unsupported, clearUncertain, failed

    var message: String {
        switch self {
        case .offline: "Aria isn’t running. Turn it on in the menu bar, then try again."
        case .storage: "Aria can’t open your dictation history. Try again later."
        case .missing: "This dictation is no longer available. Refresh history."
        case .audioUnavailable: "The recording is gone, so Aria can’t transcribe it again."
        case .projectUnavailable: "This dictation’s project folder is missing. Restore it, then transcribe again."
        case .tooLarge: "The transcript is too large for history to store."
        case .busy: "Aria is busy. Try again in a moment."
        case .cancelled: "Transcribing again was canceled."
        case .timeout: "Aria took too long to respond. Try again."
        case .queryTooLong: "Use a shorter search."
        case .unsupported: "Update and restart Aria to clear dictation history."
        case .clearUncertain: "Aria couldn’t confirm that history was cleared. Refresh history, then try again."
        case .failed: "Something went wrong. Try again."
        }
    }
}
