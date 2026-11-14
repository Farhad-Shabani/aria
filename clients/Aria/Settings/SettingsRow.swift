/// A settings row that search can find. Its page labels the row with
/// `title`, so a result names the row as the page shows it.
enum SettingsRow: CaseIterable, Identifiable {
    // In page order, pane by pane, so results list them as the page does.
    case openAtLogin
    case assistant
    case showStatusPill
    case showInlinePreview
    case showCaptions
    case speakingRate
    case storage
    case ariaSetup
    case speechModel
    case deliver
    case output
    case cleanUpText
    case muteWhileRecording
    case returnSends
    case autoStop
    case language
    case onlyTypeMyVoice
    case useProjectVocabulary
    case vocabularyFolder
    case serverRecognition
    case ariaKey
    case openAria
    case ask
    case saveHistory
    case keepAudio
    case memory
    case phonePairing
    case checkForUpdates
    case installUpdates

    var id: Self { self }

    var title: String {
        switch self {
        case .openAtLogin: "Open at login"
        case .assistant: "Assistant"
        case .showStatusPill: "Show status pill"
        case .showInlinePreview: "Show inline preview"
        case .showCaptions: "Show captions"
        case .speakingRate: "Speaking rate"
        case .storage: "Storage"
        case .ariaSetup: "Aria Setup"
        case .speechModel: "Speech model"
        case .deliver: "Deliver"
        case .output: "Output"
        case .cleanUpText: "Clean up text"
        case .muteWhileRecording: "Mute while recording"
        case .returnSends: "Return sends"
        case .autoStop: "Auto-stop"
        case .language: "Language"
        case .onlyTypeMyVoice: "Only type my voice"
        case .useProjectVocabulary: "Use project vocabulary"
        case .vocabularyFolder: "Folder"
        case .serverRecognition: "Recognize speech on your server"
        case .ariaKey: "Aria key"
        case .openAria: "Open Aria"
        case .ask: "Ask"
        case .saveHistory: "Save history"
        case .keepAudio: "Keep audio for 24 hours"
        case .memory: "Memory"
        case .phonePairing: "Phone pairing"
        case .checkForUpdates: "Automatically check for updates"
        case .installUpdates: "Automatically download and install"
        }
    }

    var keywords: [String] {
        switch self {
        case .openAtLogin: ["Startup", "login items"]
        case .assistant: ["chat", "Claude", "Codex", "model"]
        case .showStatusPill: ["On Screen", "indicator"]
        case .showInlinePreview: ["On Screen", "cursor", "ghost"]
        case .showCaptions: ["On Screen", "subtitles", "transcript"]
        case .speakingRate: ["Spoken Content", "speed", "read aloud", "voice"]
        case .storage: ["models", "disk", "space", "delete"]
        case .ariaSetup: ["setup", "permissions", "download"]
        case .speechModel: ["transcription", "engine", "download"]
        case .deliver: ["streaming", "type while speaking"]
        case .output: ["Text", "type", "paste", "copy", "clipboard"]
        case .cleanUpText: ["Text", "filler words", "um", "spoken corrections"]
        case .muteWhileRecording: ["Recording", "sound", "volume"]
        case .returnSends: ["Recording", "Enter", "send"]
        case .autoStop: ["Recording", "silence", "timeout"]
        case .language: ["locale"]
        case .onlyTypeMyVoice: ["Voice", "Your Voice", "speaker", "owner", "guest"]
        case .useProjectVocabulary: ["Vocabulary", "words", "terms"]
        case .vocabularyFolder: ["Vocabulary", "project vocabulary"]
        case .serverRecognition: ["server", "remote", "pair"]
        case .ariaKey: ["keyboard shortcut", "modifier", "dictate", "Option"]
        case .openAria: ["keyboard shortcut", "commands", "prompts", "history"]
        case .ask: ["keyboard shortcut", "assistant", "question"]
        case .saveHistory: ["History", "dictations", "transcripts", "chats"]
        case .keepAudio: ["History", "recordings", "transcribe again"]
        case .memory: ["Claude", "facts", "remember"]
        case .phonePairing: ["iPhone", "Apple Watch", "pair", "code"]
        case .checkForUpdates: ["updates", "version"]
        case .installUpdates: ["updates", "version"]
        }
    }

    var pane: SettingsPane {
        switch self {
        case .openAtLogin, .assistant, .showStatusPill, .showInlinePreview, .showCaptions, .speakingRate, .storage,
            .ariaSetup:
            .general
        case .speechModel, .deliver, .output, .cleanUpText, .muteWhileRecording, .returnSends, .autoStop: .dictation
        case .language, .onlyTypeMyVoice, .useProjectVocabulary, .vocabularyFolder, .serverRecognition: .recognition
        case .ariaKey, .openAria, .ask: .shortcuts
        case .saveHistory, .keepAudio, .memory: .privacy
        case .phonePairing: .phone
        case .checkForUpdates, .installUpdates: .about
        }
    }
}
