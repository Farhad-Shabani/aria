import Foundation

enum DictationEdits {
    static let outputModes: [Aria_Daemon_V1_OutputMode] = [.insert, .insertAndSubmit, .insertOrCopy, .copy]
    static let deliveryModes: [Aria_Daemon_V1_DeliveryMode] = [.streaming, .complete]
    static let silenceSecondsRange = 0.5...3.0
    /// Milliseconds; 0 is off.
    static let autoStopChoices: [UInt64] = [0, 3000, 5000, 10000, 15000, 20000, 30000, 45000, 60000]

    /// Copy cannot stream, so the daemon delivers it after recording stops.
    static func locksDelivery(_ mode: Aria_Daemon_V1_OutputMode) -> Bool {
        mode == .copy
    }

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

    /// The daemon accepts any whole millisecond in range, so a value set
    /// elsewhere is listed too and the menu always has a match.
    static func autoStopChoices(including current: UInt64) -> [UInt64] {
        autoStopChoices.contains(current) ? autoStopChoices : (autoStopChoices + [current]).sorted()
    }

    static func autoStopTitle(_ milliseconds: UInt64) -> String {
        guard milliseconds > 0 else { return "Off" }
        let seconds = Double(milliseconds) / 1000
        return "\(seconds.formatted(.number.precision(.fractionLength(0...1)))) s"
    }

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

extension Aria_Daemon_V1_DeliveryMode {
    var title: String {
        switch self {
        case .streaming: return "While speaking"
        case .complete: return "After recording stops"
        case .unspecified, .UNRECOGNIZED: return "Unknown"
        }
    }
}

extension Aria_Daemon_V1_PreferencePatch {
    func apply(to patch: inout Aria_Daemon_V1_PreferencePatch) {
        if hasAssistantID { patch.assistantID = assistantID }
        for entry in assistantModels {
            patch.assistantModels.removeAll { $0.assistantID == entry.assistantID }
            patch.assistantModels.append(entry)
        }
        if hasRecognitionLanguage { patch.recognitionLanguage = recognitionLanguage }
        if hasOutputMode { patch.outputMode = outputMode }
        if hasDeliveryMode { patch.deliveryMode = deliveryMode }
        if hasSilenceDurationMs { patch.silenceDurationMs = silenceDurationMs }
        if hasAutoStopMs { patch.autoStopMs = autoStopMs }
        if hasOwnVoiceOnly { patch.ownVoiceOnly = ownVoiceOnly }
        if hasDictationHotkey { patch.dictationHotkey = dictationHotkey }
        if hasAskHotkey { patch.askHotkey = askHotkey }
        if hasPromptListHotkey { patch.promptListHotkey = promptListHotkey }
        if hasProjectVocabulary { patch.projectVocabulary = projectVocabulary }
        if hasVocabularyDirectory { patch.vocabularyDirectory = vocabularyDirectory }
        if hasDictationHistory { patch.dictationHistory = dictationHistory }
        if hasHistoryAudioRetention { patch.historyAudioRetention = historyAudioRetention }
        if hasMuteOutput { patch.muteOutput = muteOutput }
        if hasPrompts { patch.prompts = prompts }
        if hasApplicationOverrides { patch.applicationOverrides = applicationOverrides }
    }

    func apply(to preferences: inout Aria_Daemon_V1_UserPreferences) {
        if hasAssistantID { preferences.assistantID = assistantID }
        for entry in assistantModels {
            preferences.assistantModels.removeAll { $0.assistantID == entry.assistantID }
            if !entry.modelID.isEmpty { preferences.assistantModels.append(entry) }
        }
        if hasRecognitionLanguage { preferences.recognitionLanguage = recognitionLanguage }
        if hasOutputMode { preferences.outputMode = outputMode }
        if hasDeliveryMode { preferences.deliveryMode = deliveryMode }
        if hasSilenceDurationMs { preferences.silenceDurationMs = silenceDurationMs }
        if hasAutoStopMs {
            // 0 turns it off on the wire; the snapshot shows none.
            if autoStopMs == 0 {
                preferences.clearAutoStopMs()
            } else {
                preferences.autoStopMs = autoStopMs
            }
        }
        if hasOwnVoiceOnly { preferences.ownVoiceOnly = ownVoiceOnly }
        if hasDictationHotkey { preferences.dictationHotkey = dictationHotkey }
        if hasAskHotkey { preferences.askHotkey = askHotkey }
        if hasPromptListHotkey { preferences.promptListHotkey = promptListHotkey }
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
        if hasPrompts { preferences.prompts = prompts }
        if hasApplicationOverrides { preferences.applicationOverrides = applicationOverrides }
    }
}
