/// One row on the palette's commands page.
struct PaletteCommand: Equatable, Identifiable {
    enum Action: Equatable {
        case showLiveTranscript
        case startTranscription
        case startDictation
        case startAsk
        case runPrompt(id: String)
        /// `nil` opens the pane Settings last showed.
        case openSettings(SettingsPane?)
        case openSetup
        case showPhonePairing
        case showHistory(HistorySection)
        case clearHistory
        case newPrompt
        /// Instructions typed or spoken for one run on the prompt input.
        case runOneOff(String)
    }

    let id: String
    let title: String
    let symbol: String
    /// The digit key that runs it while nothing is typed. Only the first nine
    /// prompts have one.
    let number: Int?
    let chord: Chord?
    /// A key that runs it from the commands page, whatever is typed.
    var key: PaletteKey?
    /// Search matches these as well as the title.
    var keywords: [String] = []
    let action: Action

    static let liveTranscript = PaletteCommand(
        id: "live-transcript", title: "Live Transcript", symbol: "waveform", number: nil, chord: nil,
        keywords: ["transcribe", "system audio", "mac audio"], action: .showLiveTranscript)
    static let newTranscription = PaletteCommand(
        id: "new-transcription", title: "Transcribe System Audio", symbol: "waveform", number: nil, chord: nil,
        keywords: ["mac audio"], action: .startTranscription)
    static let newDictation = PaletteCommand(
        id: "new-dictation", title: "New Dictation", symbol: "mic", number: nil, chord: nil,
        action: .startDictation)
    static func ask(_ assistantName: String?) -> PaletteCommand {
        PaletteCommand(
            id: "new-chat", title: "Ask \(assistantName ?? "Your Assistant")", symbol: "bubble.left", number: nil,
            chord: nil, keywords: ["question", "chat"], action: .startAsk)
    }
    static func history(_ section: HistorySection) -> PaletteCommand {
        PaletteCommand(
            id: "history-\(section)", title: section.historyTitle, symbol: section.symbol, number: nil, chord: nil,
            keywords: ["history"], action: .showHistory(section))
    }
    /// No key: clearing everything takes typing its name and confirming.
    static let clearHistory = PaletteCommand(
        id: "clear-history", title: "Clear All History…", symbol: "trash", number: nil, chord: nil,
        action: .clearHistory)
    static let newPrompt = PaletteCommand(
        id: "new-prompt", title: "New Prompt…", symbol: "square.and.pencil", number: nil, chord: nil,
        action: .newPrompt)
    static func oneOff(_ instructions: String) -> PaletteCommand {
        PaletteCommand(
            id: "one-off", title: "Run “\(instructions)”", symbol: "sparkles", number: nil, chord: nil,
            action: .runOneOff(instructions))
    }
    static let settings = PaletteCommand(
        id: "settings", title: "Settings…", symbol: "gearshape", number: nil, chord: nil,
        key: .settings, action: .openSettings(nil))
    static let finishSetup = PaletteCommand(
        id: "finish-setup", title: "\(SettingsPane.finishSetup.title)…", symbol: SettingsPane.finishSetup.symbol,
        number: nil, chord: nil, keywords: ["setup"], action: .openSettings(.finishSetup))
    static let pairPhone = PaletteCommand(
        id: "pair-phone", title: "Pair Your iPhone", symbol: SettingsPane.phone.symbol, number: nil, chord: nil,
        keywords: ["phone", "pairing", "qr code"], action: .showPhonePairing)
    static let setUp = PaletteCommand(
        id: "set-up", title: "Set Up Aria…", symbol: "wand.and.stars", number: nil, chord: nil,
        keywords: ["setup"], action: .openSetup)
}
