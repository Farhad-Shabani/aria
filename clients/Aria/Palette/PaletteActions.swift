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
    var history = HistoryWork()
    /// Whether an app was in front when the palette opened, to type into.
    var hasFrontApp = false

    var all: [PaletteAction] {
        guard model.renamingID == nil else { return [] }
        switch model.page {
        case .commands:
            guard model.input == nil else { return [] }
            switch model.selected {
            case .entry(let entry): return actions(for: entry, isOpen: false)
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

    func typesOnReturn(_ item: PaletteItem) -> Bool {
        hasFrontApp && item.isTypable && !history.isClearing
    }

    /// Names what Return does with the selected row.
    func returnTitle(typingInto app: String?) -> String? {
        switch model.selected {
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
