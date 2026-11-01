import Foundation

enum DictationEdits {
    static let outputModes: [Aria_Daemon_V1_OutputMode] = [.insert, .insertAndSubmit, .insertOrCopy, .copy]
    static let deliveryModes: [Aria_Daemon_V1_DeliveryMode] = [.streaming, .complete]
    static let silenceSecondsRange = 0.5...3.0
    static let speakingRateRange = 0.25...4.0
    static let speakingRateStep = 0.05
    /// Milliseconds; 0 is off.
    static let autoStopChoices: [UInt64] = [0, 3000, 5000, 10000, 15000, 20000, 30000, 45000, 60000]

    /// The wire contract on `Settings.delivery_mode`: Copy cannot stream.
    static func locksDelivery(_ mode: Aria_Daemon_V1_OutputMode) -> Bool {
        mode == .copy
    }

    /// The delivery in effect: Complete while output copies, otherwise the
    /// stored choice. Copy leaves the stored choice alone, so it comes back
    /// when output changes again.
    static func effectiveDelivery(in settings: Aria_Daemon_V1_Settings) -> Aria_Daemon_V1_DeliveryMode {
        locksDelivery(settings.outputMode) ? .complete : settings.deliveryMode
    }

    /// Whole milliseconds; the slider already keeps the value in the
    /// daemon's 100 ms to 10 s window.
    static func silenceMilliseconds(seconds: Double) -> UInt64 {
        UInt64((seconds * 1000).rounded())
    }

    static func roundedSpeakingRate(_ rate: Double) -> Double {
        let rounded = (rate / speakingRateStep).rounded() * speakingRateStep
        return min(max(rounded, speakingRateRange.lowerBound), speakingRateRange.upperBound)
    }

    /// Spoken corrections rewrite the whole transcript, which a streamed
    /// dictation has already typed.
    static func allowsSpokenCorrections(_ mode: Aria_Daemon_V1_DeliveryMode) -> Bool {
        mode == .complete
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

extension Aria_Daemon_V1_SettingsPatch {
    func apply(to patch: inout Aria_Daemon_V1_SettingsPatch) {
        if hasOutputMode { patch.outputMode = outputMode }
        if hasDeliveryMode { patch.deliveryMode = deliveryMode }
        if hasSilenceDurationMs { patch.silenceDurationMs = silenceDurationMs }
        if hasAutoStopMs { patch.autoStopMs = autoStopMs }
        if hasMuteSound { patch.muteSound = muteSound }
        if hasCleanup { patch.cleanup = cleanup }
        if hasBacktrack { patch.backtrack = backtrack }
        if hasOwnVoiceOnly { patch.ownVoiceOnly = ownVoiceOnly }
        if hasVocabularyEnabled { patch.vocabularyEnabled = vocabularyEnabled }
        if hasVocabularyDir { patch.vocabularyDir = vocabularyDir }
        if hasHistoryEnabled { patch.historyEnabled = historyEnabled }
        if hasHistoryKeepAudio { patch.historyKeepAudio = historyKeepAudio }
        if hasAssistantID { patch.assistantID = assistantID }
        for entry in assistantModels {
            patch.assistantModels.removeAll { $0.assistantID == entry.assistantID }
            patch.assistantModels.append(entry)
        }
        if hasMemoryEnabled { patch.memoryEnabled = memoryEnabled }
        if hasPrompts { patch.prompts = prompts }
        if hasApplicationOverrides { patch.applicationOverrides = applicationOverrides }
        if hasAriaKey { patch.ariaKey = ariaKey }
        if hasAskShortcut { patch.askShortcut = askShortcut }
        if hasPromptListShortcut { patch.promptListShortcut = promptListShortcut }
        if hasSttLanguage { patch.sttLanguage = sttLanguage }
        if hasTtsRate { patch.ttsRate = ttsRate }
    }

    func apply(to settings: inout Aria_Daemon_V1_Settings) {
        if hasOutputMode { settings.outputMode = outputMode }
        if hasDeliveryMode { settings.deliveryMode = deliveryMode }
        if hasSilenceDurationMs { settings.silenceDurationMs = silenceDurationMs }
        if hasAutoStopMs {
            // 0 turns it off on the wire; the snapshot shows none.
            if autoStopMs == 0 {
                settings.clearAutoStopMs()
            } else {
                settings.autoStopMs = autoStopMs
            }
        }
        if hasMuteSound { settings.muteSound = muteSound }
        if hasCleanup { settings.cleanup = cleanup }
        if hasBacktrack { settings.backtrack = backtrack }
        if hasOwnVoiceOnly { settings.ownVoiceOnly = ownVoiceOnly }
        if hasVocabularyEnabled { settings.vocabularyEnabled = vocabularyEnabled }
        if hasVocabularyDir {
            // Empty clears on the wire; the snapshot shows none.
            if vocabularyDir.isEmpty {
                settings.clearVocabularyDir()
            } else {
                settings.vocabularyDir = vocabularyDir
            }
        }
        if hasHistoryEnabled { settings.historyEnabled = historyEnabled }
        if hasHistoryKeepAudio { settings.historyKeepAudio = historyKeepAudio }
        if hasAssistantID { settings.assistantID = assistantID }
        for entry in assistantModels {
            settings.assistantModels.removeAll { $0.assistantID == entry.assistantID }
            if !entry.modelID.isEmpty { settings.assistantModels.append(entry) }
        }
        if hasMemoryEnabled { settings.memoryEnabled = memoryEnabled }
        if hasPrompts { settings.prompts = prompts }
        if hasApplicationOverrides { settings.applicationOverrides = applicationOverrides }
        if hasAriaKey { settings.ariaKey = ariaKey }
        if hasAskShortcut { settings.askShortcut = askShortcut }
        if hasPromptListShortcut { settings.promptListShortcut = promptListShortcut }
        if hasSttLanguage { settings.sttLanguage = sttLanguage }
        if hasTtsRate { settings.ttsRate = ttsRate }
    }
}
