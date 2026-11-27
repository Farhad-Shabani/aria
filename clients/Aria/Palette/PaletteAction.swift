import Foundation

/// Something to do on the page or with its selected row, and the key that does it from anywhere on
/// the page. The footer shows the everyday ones. ⌘K lists all.
struct PaletteAction: Identifiable, Equatable {
    enum Kind: Equatable {
        /// A row's entry, which holds only the start of its text.
        case copyEntry(HistoryItem)
        /// The open entry, or the result the keys rest on.
        case copyDetail
        case copyText(String)
        case copyTranscript
        case runPrompt(on: HistoryItem)
        case transcribeAgain
        case rename(HistoryItem)
        case delete(HistoryItem)
        case open(PaletteItem)
        case editPrompt(id: String)
        case saveAsPrompt(instructions: String)
        case retry
        case stop
        case typeText(String)
        /// Stops a chat's reply, on its page or from its row.
        case stopAsk(ChatKey)
        case deleteChat(ChatKey)
        case renameChat(ChatKey)
        case copyPairingCode(PairingInvite, expiresAt: Date)
        case turnOnPhonePairing
        case addWord(String)
        case removeWord(String)
        /// A Mac Audio entry's transcript file.
        case showInFinder(URL)
        /// Asks where, then saves it there as the person's own file.
        case saveTranscript(UnsavedTranscript)
        /// Asks first, since it can't be undone.
        case discardTranscript(UnsavedTranscript)
    }

    let kind: Kind
    var isInFooter = false
    var isEnabled = true

    var id: String {
        switch kind {
        case .copyEntry, .copyDetail, .copyText, .copyTranscript, .copyPairingCode: "copy"
        case .runPrompt: "run-prompt"
        case .transcribeAgain: "transcribe-again"
        case .rename, .renameChat: "rename"
        case .delete: "delete"
        case .open: "open"
        case .editPrompt: "edit-prompt"
        case .saveAsPrompt: "save-as-prompt"
        case .retry: "retry"
        case .stop, .stopAsk: "stop"
        case .typeText: "type"
        case .deleteChat: "delete-chat"
        case .turnOnPhonePairing: "turn-on-phone-pairing"
        case .addWord: "add-word"
        case .removeWord: "remove-word"
        case .showInFinder: "show-in-finder"
        case .saveTranscript: "save-transcript"
        case .discardTranscript: "discard-transcript"
        }
    }

    var title: String {
        switch kind {
        case .copyEntry, .copyDetail, .copyText, .copyTranscript: "Copy"
        case .runPrompt: "Run Prompt"
        case .transcribeAgain: "Transcribe Again"
        case .rename, .renameChat: "Rename"
        case .delete: "Delete…"
        case .open: "Open"
        case .editPrompt: "Edit Prompt"
        case .saveAsPrompt: "Save as Prompt"
        case .retry: "Try Again"
        case .stop, .stopAsk: "Stop"
        case .typeText: "Type"
        case .deleteChat: "Delete Chat…"
        case .copyPairingCode: "Copy Code"
        case .turnOnPhonePairing: "Turn On"
        case .addWord: "Add to Dictionary"
        case .removeWord: "Remove"
        case .showInFinder: "Show in Finder"
        case .saveTranscript: "Save…"
        case .discardTranscript: "Discard…"
        }
    }

    var key: PaletteKey {
        switch kind {
        case .copyEntry, .copyDetail, .copyText, .copyTranscript, .copyPairingCode: .copy
        case .runPrompt: .runPrompt
        case .transcribeAgain: .transcribeAgain
        case .rename, .renameChat: .rename
        case .delete: .delete
        case .open: .open
        case .editPrompt: .editPrompt
        case .saveAsPrompt: .saveAsPrompt
        case .retry: .retry
        case .stop, .stopAsk: .stop
        case .typeText: .typeText
        case .deleteChat: .delete
        case .turnOnPhonePairing: .turnOn
        case .addWord: .addWord
        case .removeWord: .delete
        case .showInFinder: .showInFinder
        case .saveTranscript: .saveTranscript
        case .discardTranscript: .delete
        }
    }
}
