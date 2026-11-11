/// What can be done on the palette's page or with its selected row, decided
/// from the state alone. The controller does them.
struct PaletteActions {
    let model: PaletteModel
    /// The run the result page shows.
    var result: PaletteResult?
    var isSystemTranscriptionRunning = false
    var isStopping = false
    var hasTranscript = false
    /// The entry the entry page shows.
    var detail: HistoryItem?
    /// The chat the chat page shows.
    var chat: Chat?
    /// The chat the selected row replies in, which it can stop.
    var replying: Chat?
    var history = HistoryWork()
    /// Whether an app was in front when the palette opened, to type into.
    var hasFrontApp = false

    var all: [PaletteAction] {
        guard !model.isRenaming else { return [] }
        switch model.page {
        case .commands:
            guard model.input == nil else { return [] }
            switch model.selected {
            case .entry(let entry):
                return actions(for: entry, isOpen: false) + stop(replying)
            case .chat(let chat):
                return actions(for: chat, isReplying: replying != nil) + stop(replying)
            case .command(let command) where command.action == .showLiveTranscript: return [stop]
            case .command(let command):
                guard case .runPrompt(let id) = command.action else { return [] }
                return [PaletteAction(kind: .editPrompt(id: id), isInFooter: true)]
            case .result(let result):
                return [PaletteAction(kind: .copyText(result.text)), PaletteAction(kind: .open(.result(result)))]
            case nil: return []
            }
        case .prompt:
            return []
        case .chat:
            return chat.map(chatActions) ?? []
        case .entry:
            return detail.map { actions(for: $0, isOpen: true) } ?? []
        case .liveTranscript:
            let copy = PaletteAction(kind: .copyTranscript, isInFooter: true, isEnabled: hasTranscript)
            return isSystemTranscriptionRunning ? [copy, stop] : [copy]
        case .result:
            switch result?.state {
            case .replied(let text)?:
                let copy = PaletteAction(kind: .copyText(text), isInFooter: true)
                guard case .oneOff(let instructions)? = result?.source else { return [copy] }
                return [copy, PaletteAction(kind: .saveAsPrompt(instructions: instructions), isInFooter: true)]
            case .failed(let failure)? where failure.canRetry:
                return [PaletteAction(kind: .retry, isInFooter: true)]
            case .failed?, .running?, nil:
                return []
            }
        }
    }

    /// A prompt, a rename, and a delete each need the chat's saved asks, so none shows while
    /// history is off.
    private func chatActions(_ chat: Chat) -> [PaletteAction] {
        var actions: [PaletteAction] = []
        let selection = model.messageSelection
        if let text = selection.text(in: chat.asks) {
            actions.append(PaletteAction(kind: .copyText(text), isInFooter: true))
            // Kept out of the footer. With no follow-up typed, the footer shows Return typing the text.
            if hasFrontApp { actions.append(PaletteAction(kind: .typeText(text))) }
        }
        let ask = selection.focusedAsk(in: chat.asks)
        if chat.canStop, let key = chat.key { actions.append(PaletteAction(kind: .stopAsk(key), isInFooter: true)) }
        guard model.keepsHistory else { return actions }
        if let entry = ask.flatMap(chat.entry(of:)) {
            actions.append(PaletteAction(kind: .runPrompt(on: entry), isEnabled: model.canAttach))
        }
        guard let key = chat.key else { return actions }
        actions.append(
            PaletteAction(kind: .renameChat(key), isEnabled: !history.isClearing))
        actions.append(
            PaletteAction(
                kind: .deleteChat(key),
                isEnabled: !chat.isReplying && !history.isClearing && history.deletingID == nil))
        return actions
    }

    /// A replying chat refuses a delete, as on its page.
    private func actions(for chat: SavedChat, isReplying: Bool) -> [PaletteAction] {
        var actions: [PaletteAction] = []
        if chat.newest.canCopy {
            actions.append(PaletteAction(kind: .copyEntry(chat.newest), isEnabled: !history.isClearing))
        }
        actions.append(
            PaletteAction(
                kind: .renameChat(chat.key),
                isEnabled: !history.isClearing))
        actions.append(
            PaletteAction(
                kind: .deleteChat(chat.key), isEnabled: !isReplying && history.canDelete(chat.newest)))
        actions.append(PaletteAction(kind: .open(.chat(chat))))
        return actions
    }

    /// Stop for a row whose chat replies off its page, once the daemon has named it.
    private func stop(_ chat: Chat?) -> [PaletteAction] {
        guard let chat, chat.canStop, let key = chat.key else { return [] }
        return [PaletteAction(kind: .stopAsk(key), isInFooter: true)]
    }

    func typesOnReturn(_ item: PaletteItem) -> Bool {
        hasFrontApp && item.isTypable && !history.isClearing
    }

    enum FollowUpReturn: Equatable {
        /// Sends the follow-up in the chat on screen.
        case send
        /// Starts a chat that quotes the text on screen, from history entry `entry`.
        case start(quote: String, entry: UInt64)
        case type(String)
    }

    /// What Return does on a page with a follow-up field. With something typed it sends or starts
    /// a chat. Otherwise it types the focused text.
    var followUpReturn: FollowUpReturn? {
        let isTyped = !model.followUp.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        switch model.page {
        case .chat:
            if isTyped { return .send }
            guard hasFrontApp, let asks = chat?.asks, let text = model.messageSelection.text(in: asks)
            else { return nil }
            return .type(text)
        case .entry, .result:
            // Too long to quote turns the field off, so Return does nothing.
            guard let quote = followUpQuote, quote.utf8.count <= ChatAsk.quoteLimit,
                let entry = model.page == .entry ? detail?.id : result?.input.id
            else { return nil }
            if isTyped { return .start(quote: quote, entry: entry) }
            return hasFrontApp ? .type(quote) : nil
        case .commands, .liveTranscript, .prompt:
            return nil
        }
    }

    /// What a follow-up on the entry or result page quotes: the focused result, else the entry.
    /// A transcript counts only when read in full. A chat's reply has none, since its chat takes
    /// follow-ups.
    var followUpQuote: String? {
        let text: String
        switch model.page {
        case .entry:
            guard let detail, detail.status != .deletionPending else { return nil }
            if let result = model.focusedResult(on: detail.id) {
                text = result.text
            } else {
                switch detail.kind {
                case .ask: return nil
                case .systemTranscription(_, let read) where read != .whole: return nil
                case .dictation, .systemTranscription: text = detail.copyText
                }
            }
        case .result:
            guard case .replied(let reply)? = result?.state else { return nil }
            text = reply
        case .commands, .chat, .liveTranscript, .prompt:
            return nil
        }
        return text.contains { !$0.isWhitespace } ? text : nil
    }

    /// Whether the entry or result page's text fits in a chat's quote. `nil` with no text to quote.
    var quoteFits: Bool? {
        followUpQuote.map { $0.utf8.count <= ChatAsk.quoteLimit }
    }

    func returnTitle(typingInto app: String?) -> String? {
        if model.page == .chat || model.page == .entry || model.page == .result {
            return switch followUpReturn {
            case .send?: "Send"
            case .start?: "Ask"
            case .type?: app.map { "Type into \($0)" }
            case nil: nil
            }
        }
        return switch model.selected {
        case .command(let command):
            switch command.action {
            case .showLiveTranscript: "Show Transcript"
            case .startTranscription, .startDictation, .startAsk: "Start"
            case .runPrompt, .runOneOff: "Run Prompt"
            case .openSettings, .openSetup, .newPrompt: "Open"
            case .clearHistory: "Clear…"
            case .showHistory: "Show"
            }
        case .some(let item):
            if typesOnReturn(item), let app { "Type into \(app)" } else { "Open" }
        case nil: nil
        }
    }

    private var stop: PaletteAction {
        PaletteAction(kind: .stop, isInFooter: true, isEnabled: !isStopping)
    }

    /// `isOpen` when the entry's own page shows, where it can be transcribed again.
    private func actions(for entry: HistoryItem, isOpen: Bool) -> [PaletteAction] {
        var actions: [PaletteAction] = []
        if entry.canCopy {
            actions.append(
                PaletteAction(
                    kind: isOpen ? .copyDetail : .copyEntry(entry), isInFooter: isOpen, isEnabled: !history.isClearing))
        }
        actions.append(PaletteAction(kind: .runPrompt(on: entry), isInFooter: true, isEnabled: model.canAttach))
        if isOpen, entry.canRegenerate || entry.isRegenerating || history.regeneratingID == entry.id {
            actions.append(PaletteAction(kind: .transcribeAgain, isEnabled: history.canRegenerate(entry)))
        }
        if entry.canRename {
            actions.append(PaletteAction(kind: .rename(entry), isEnabled: history.canRename(entry.id)))
        }
        actions.append(PaletteAction(kind: .delete(entry), isEnabled: history.canDelete(entry)))
        if !isOpen { actions.append(PaletteAction(kind: .open(.entry(entry)))) }
        return actions
    }
}
