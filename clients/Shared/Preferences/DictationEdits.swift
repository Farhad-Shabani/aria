import Foundation

/// Rules a client applies before a patch leaves the app, shared by the Mac
/// window and the phone screen.
enum DictationEdits {
    static let outputModes: [Aria_Daemon_V1_OutputMode] = [.insert, .insertAndSubmit, .insertOrCopy, .copy]
    static let silenceSecondsRange = 0.5...3.0

    /// Copy cannot stream: the daemon refuses to start such a dictation.
    static func locksDelivery(_ mode: Aria_Daemon_V1_OutputMode) -> Bool {
        mode == .copy
    }

    /// An output that locks delivery switches it to complete in the same patch.
    static func setOutput(_ mode: Aria_Daemon_V1_OutputMode, in patch: inout Aria_Daemon_V1_PreferencePatch) {
        patch.outputMode = mode
        if locksDelivery(mode) {
            patch.deliveryMode = .complete
        }
    }

    /// Whole milliseconds; the slider already keeps the value in the
    /// daemon's 100 ms to 10 s window.
    static func silenceMilliseconds(seconds: Double) -> UInt64 {
        UInt64((seconds * 1000).rounded())
    }

    /// Detection, English, and whatever else the daemon already stores,
    /// since the API accepts any language tag.
    static func languageChoices(including current: String) -> [String] {
        let standard = ["auto", "en"]
        if standard.contains(current) || current.isEmpty {
            return standard
        }
        return standard + [current]
    }

    static func languageTitle(_ code: String) -> String {
        switch code {
        case "auto": return "Automatic"
        case "en": return "English"
        default: return Locale.current.localizedString(forIdentifier: code) ?? code
        }
    }
}

extension Aria_Daemon_V1_OutputMode {
    var title: String {
        switch self {
        case .insert: return "Insert at the cursor"
        case .insertAndSubmit: return "Insert, then press Return"
        case .insertOrCopy: return "Insert, or copy if that fails"
        case .copy: return "Copy to the clipboard"
        case .unspecified, .UNRECOGNIZED: return "Unknown"
        }
    }
}

extension Aria_Daemon_V1_PreferencePatch {
    /// Lays the set fields over a snapshot, as the daemon will.
    func apply(to preferences: inout Aria_Daemon_V1_UserPreferences) {
        if hasRecognitionLanguage { preferences.recognitionLanguage = recognitionLanguage }
        if hasOutputMode { preferences.outputMode = outputMode }
        if hasDeliveryMode { preferences.deliveryMode = deliveryMode }
        if hasSilenceDurationMs { preferences.silenceDurationMs = silenceDurationMs }
        if hasOwnVoiceOnly { preferences.ownVoiceOnly = ownVoiceOnly }
        if hasDictationHotkey { preferences.dictationHotkey = dictationHotkey }
        if hasAskHotkey { preferences.askHotkey = askHotkey }
        if hasProjectVocabulary { preferences.projectVocabulary = projectVocabulary }
        if hasVocabularyDirectory {
            // Empty clears on the wire; the snapshot shows none.
            if vocabularyDirectory.isEmpty {
                preferences.clearVocabularyDirectory()
            } else {
                preferences.vocabularyDirectory = vocabularyDirectory
            }
        }
        if hasDictationHistory { preferences.dictationHistory = dictationHistory }
        if hasHistoryAudioRetention { preferences.historyAudioRetention = historyAudioRetention }
        if hasMuteOutput { preferences.muteOutput = muteOutput }
    }
}
