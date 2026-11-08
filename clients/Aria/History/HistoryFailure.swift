enum HistoryFailure: Error, Equatable {
    case offline, storage, missing, audioUnavailable, projectUnavailable, tooLarge, busy, cancelled,
        timeout, queryTooLong, nameTooLong, tooLongToType, copyFailed, unsupported, clearUncertain, failed

    init(_ failure: DaemonFailure) {
        self =
            switch failure {
            case _ where failure.isOffline: .offline
            case .refused(.historyUnavailable): .storage
            case .refused(.historyMissing): .missing
            case .refused(.historyAudioUnavailable): .audioUnavailable
            case .refused(.payloadTooLarge): .tooLarge
            case .refused(.historyProjectUnavailable): .projectUnavailable
            case .refused(.busy): .busy
            case .refused(.cancelled): .cancelled
            case .timeout: .timeout
            case .cancelled: .cancelled
            case .unimplemented: .unsupported
            case .refused, .unavailable, .other, .invalidResponse: .failed
            }
    }

    var message: String {
        switch self {
        case .offline: DaemonFailure.offlineMessage
        case .storage: "Aria can’t open your history. Try again later."
        case .missing: "This entry is no longer in history."
        case .audioUnavailable: "The recording is gone, so Aria can’t transcribe it again."
        case .projectUnavailable: "This dictation’s project folder is missing. Restore it, then transcribe again."
        case .tooLarge: "The text is too large for history to store."
        case .busy: "Aria is busy. Try again in a moment."
        case .cancelled: "Transcribing again was canceled."
        case .timeout: "Aria took too long to respond. Try again."
        case .queryTooLong: "Use a shorter search."
        case .nameTooLong: "Use a shorter name."
        case .tooLongToType: "This is too long to type. Copy it instead."
        case .copyFailed: "Aria couldn’t copy the text. Try again."
        case .unsupported: "Update and restart Aria to clear history."
        case .clearUncertain: "Aria couldn’t confirm that history was cleared. Refresh history, then try again."
        case .failed: "Something went wrong. Try again."
        }
    }
}
