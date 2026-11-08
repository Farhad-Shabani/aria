import Carbon.HIToolbox
import Combine
import SwiftUI
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Palette")

/// Opens and closes the palette, and runs what is chosen in it.
@MainActor
final class PaletteController: ObservableObject {
    @Published var model = PaletteModel()
    let promptRun: PaletteRun
    /// The entry waiting on its delete to be confirmed.
    @Published var deletion: HistoryItem?
    /// Where Return types text: the app in front when the palette opened.
    private(set) var frontApp: NSRunningApplication?
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    let listen: ListenStatus
    let history: HistoryState

    private let prompts: PromptHotkeys
    private let ask: PressController
    private let openSettings: @MainActor () -> Void
    /// Shows in the pill how typing went, once the palette is gone.
    private let delivered: @MainActor (Result<Aria_Daemon_V1_DeliveryOutcome, any Error>) -> Void
    private lazy var panel = PalettePanel(
        resignedKey: { [weak self] in self?.close() },
        pressed: { [weak self] event in self?.press(event) ?? false })
    private var gesture = PaletteGesture()
    private var chordChange: AnyCancellable?
    private var historySettingChange: AnyCancellable?
    private var listenChange: AnyCancellable?
    private var entriesChange: AnyCancellable?
    private var filterChange: AnyCancellable?
    private var pageChange: AnyCancellable?
    private var replyChange: AnyCancellable?
    private var promptsChange: AnyCancellable?
    /// Where Edit Prompt finds a prompt. `nil` until the app tracks the settings.
    private var settings: DaemonSettings?
    /// The prompts that are on, in Settings order, as the settings last said.
    private var enabledPrompts: [Aria_Daemon_V1_Prompt] = []
    private var savingDraft: Task<Void, Never>?
    /// Fetches an entry's text to type. Closing the palette cancels it.
    private var typing: Task<Void, Never>?
    /// Counts the times the palette opened, so work that outlives one opening
    /// leaves the next alone.
    private var openings = 0

    init(
        prompts: PromptHotkeys, ask: PressController, listen: ListenStatus, history: HistoryState,
        events: DaemonEvents, openSettings: @escaping @MainActor () -> Void,
        delivered: @escaping @MainActor (Result<Aria_Daemon_V1_DeliveryOutcome, any Error>) -> Void
    ) {
        self.prompts = prompts
        self.ask = ask
        self.listen = listen
        self.history = history
        self.openSettings = openSettings
        self.delivered = delivered
        promptRun = PaletteRun(
            effects: .init(
                run: { entry, source in try await ChatClient.runPromptForReply(entry: entry, source: source) },
                cancel: { ticket in try await ChatClient.cancel(ticket: ticket) }))
        events.onChat { [weak self] event in self?.promptRun.apply(event) }
        // A run's reply is saved under its entry, so the entry's count and
        // open thread catch up.
        replyChange =
            promptRun.$result
            .compactMap { result -> UInt64? in
                guard let result, result.source != nil, case .replied = result.state else { return nil }
                return result.ticket
            }
            .removeDuplicates()
            .sink { [weak self] _ in
                guard let self else { return }
                history.refresh()
                reloadThread()
            }
        pageChange =
            $model
            .map(\.page)
            .removeDuplicates()
            .sink { [weak self] _ in self?.history.forgetCopied() }
        listenChange = listen.$isRunning
            .removeDuplicates()
            .sink { [weak self] isRunning in self?.model.isListening = isRunning }
        entriesChange = history.$days
            .sink { [weak self] days in self?.model.replace(entries: days.flatMap(\.items)) }
        // The model owns what the list is narrowed to. History only follows.
        filterChange =
            $model
            .map { ($0.query, $0.section) }
            .removeDuplicates(by: ==)
            .sink { [weak self] query, section in self?.history.filter(query: query, section: section) }
    }

    func track(_ settings: DaemonSettings) {
        self.settings = settings
        promptsChange = settings.$snapshot
            .compactMap { $0?.prompts.entries }
            .removeDuplicates()
            .sink { [weak self] entries in
                guard let self else { return }
                enabledPrompts = entries.filter(\.enabled)
                model.canAddPrompt = entries.count < PromptEdits.maximumPrompts
                model.replace(prompts: enabledPrompts)
            }
        chordChange = settings.$snapshot
            .map { $0?.promptListShortcut.storedChord }
            .sink { [weak self] chord in
                Task { @MainActor in self?.chord.bind(chord) }
            }
        historySettingChange = settings.$snapshot
            .map { $0?.historyEnabled ?? false }
            .removeDuplicates()
            .sink { [weak self] keepsHistory in self?.model.keepsHistory = keepsHistory }
    }

    func press() {
        switch gesture.press(isOpen: panel.isVisible, isBusy: { prompts.isBusy($0) }) {
        case .ignore: break
        case .open: open()
        case .close: close()
        }
    }

    func run(_ command: PaletteCommand) {
        switch command.action {
        case .showLiveTranscript:
            model.show(.liveTranscript)
        case .startTranscription:
            startTranscription()
        case .startDictation:
            close()
            Task { await DictationClient.dictateToHistory() }
        case .startAsk:
            // The palette closes before recording starts, so the ask begins with the app in front,
            // as the Ask shortcut does.
            close()
            ask.press()
        case .showHistory(let section):
            model.show(section: section)
        case .clearHistory:
            model.isConfirmingClear = true
        case .newPrompt:
            model.open(PromptDraft())
        case .runPrompt(let id) where model.input != nil:
            if let input = model.input { runPrompt(id, on: input) }
        case .runOneOff(let instructions):
            guard let input = model.input else { return }
            history.forgetCopied()
            promptRun.start(.oneOff(instructions: instructions), named: instructions, on: input)
            model.show(.result)
        case .runPrompt(let id):
            close()
            gesture.chose(id)
            prompts.press(id)
        case .openSettings:
            // The palette's panel never activates Aria, so macOS declines a
            // plain activate() while another app is in front. Settings then
            // opens behind it.
            NSApplication.shared.activate(ignoringOtherApps: true)
            close()
            openSettings()
        }
    }

    func choose(_ item: PaletteItem) {
        switch item {
        case .command(let command):
            run(command)
        case .entry(let entry):
            history.select(entry.id)
            model.open(entry: entry.id)
            loadThread()
        case .result(let result):
            guard let entry = model.threadEntry else { return }
            history.forgetCopied()
            promptRun.show(saved: result, on: entry)
            model.show(.result)
        }
    }

    /// Return: saved text is typed into the app in front, as in a clipboard
    /// history. Anything else, or with no app to type into, is chosen.
    func chooseSelected() {
        guard let item = model.selected else { return }
        guard decisions.typesOnReturn(item) else { return choose(item) }
        switch item {
        case .result(let result):
            deliver(result.text)
        case .entry(let entry):
            guard typing == nil else { return }
            typing = Task {
                defer { typing = nil }
                guard let text = await history.typingText(of: entry.id), !Task.isCancelled else { return }
                deliver(text)
            }
        case .command:
            break
        }
    }

    var returnTitle: String? {
        decisions.returnTitle(typingInto: frontApp?.localizedName)
    }

    /// Opens the selected entry's thread. Returns whether there was one to open.
    func expandThread() -> Bool {
        guard let entry = model.threadEntry, !model.isThreadOpen(of: entry.id) else { return false }
        toggleThread(of: entry)
        return true
    }

    func toggleThread(of entry: HistoryItem) {
        guard !model.isThreadOpen(of: entry.id) else {
            model.foldThread()
            return
        }
        model.requestThread(of: entry.id, opens: true)
        loadThread()
    }

    private func reloadThread() {
        guard let thread = model.thread else { return }
        model.requestThread(of: thread.entryID, opens: thread.isOpen)
        loadThread()
    }

    /// Reads what the model asked for. The model drops a read that a later
    /// request overtook.
    private func loadThread() {
        guard let entry = model.threadRequest?.entryID else { return }
        Task {
            guard let results = await history.thread(of: entry) else { return }
            model.replace(results: results, of: entry)
        }
    }

    func saveTitle() {
        guard let id = model.renamingID else { return }
        let title = model.renamingTitle
        model.stopRenaming()
        history.rename(id, to: title)
    }

    /// What can be done on the page or with its selected row, built at each
    /// key press so it acts on what is selected then.
    var actions: [PaletteAction] {
        decisions.all
    }

    /// Whether the ⌘K list holds more than the footer shows.
    var hasMenu: Bool {
        actions.contains { !$0.isInFooter }
    }

    private var decisions: PaletteActions {
        PaletteActions(
            model: model, result: promptRun.result, isListening: listen.isRunning, isStopping: listen.isStopping,
            hasTranscript: !listen.transcript.lines.isEmpty, detail: history.detail, history: history.work,
            hasFrontApp: frontApp != nil)
    }

    func perform(_ action: PaletteAction) {
        switch action.kind {
        case .copyEntry(let entry): history.copy(entry.id)
        case .copyDetail:
            if let result = focusedResult { history.copy(text: result.text) } else { history.copy() }
        case .copyText(let text): history.copy(text: text)
        case .copyTranscript: history.copy(text: listen.transcript.text)
        case .runPrompt(let entry): model.attach(entry)
        case .transcribeAgain: history.regenerate()
        case .rename(let entry): model.startRenaming(entry)
        case .delete(let entry): deletion = entry
        case .open(let item): choose(item)
        case .editPrompt(let id): editPrompt(id)
        case .saveAsPrompt(let instructions):
            model.open(PromptDraft(instructions: instructions, kind: .compose), replacing: true)
        case .retry: promptRun.retry()
        case .stop: stopTranscription()
        }
    }

    func toggleMenu() {
        model.toggleMenu(count: hasMenu ? actions.count : 0)
    }

    func runMenuAction(at index: Int) {
        let actions = actions
        model.closeMenu()
        guard actions.indices.contains(index), actions[index].isEnabled else { return }
        perform(actions[index])
    }

    private func press(_ event: NSEvent) -> Bool {
        if let index = model.menuIndex {
            return pressMenu(event, at: index)
        }
        if PaletteKey.escape.matches(event) {
            escape()
            return true
        }
        if model.page == .prompt, PaletteKey.save.matches(event) {
            saveDraft()
            return true
        }
        if model.page == .entry, let entry = history.detail?.id,
            let step = Self.step(for: event) ?? Self.arrowStep(for: event)
        {
            // The ends of the thread still take the keys, so they don't beep.
            model.moveDetailFocus(by: step, on: entry)
            return model.renamingID == nil
        }
        if let step = Self.step(for: event) {
            guard model.page == .commands else { return false }
            model.moveSelection(by: step)
            return true
        }
        if PaletteKey.nextSection.matches(event) || PaletteKey.previousSection.matches(event) {
            return model.cycleSection(by: PaletteKey.nextSection.matches(event) ? 1 : -1)
        }
        if let command = model.command(pressing: event) {
            run(command)
            return true
        }
        if runNumber(event) { return true }
        if PaletteKey.actions.matches(event), hasMenu {
            toggleMenu()
            return true
        }
        guard let action = actions.first(where: { $0.key.matches(event) }) else { return false }
        if action.isEnabled { perform(action) }
        return true
    }

    private func pressMenu(_ event: NSEvent, at index: Int) -> Bool {
        let count = actions.count
        if let step = Self.step(for: event) {
            model.moveMenu(by: step, count: count)
            return true
        }
        switch Int(event.keyCode) {
        case kVK_UpArrow: model.moveMenu(by: -1, count: count)
        case kVK_DownArrow: model.moveMenu(by: 1, count: count)
        case kVK_Return, kVK_ANSI_KeypadEnter: runMenuAction(at: index)
        case _ where PaletteKey.escape.matches(event) || PaletteKey.actions.matches(event): model.closeMenu()
        default:
            model.closeMenu()
            return press(event)
        }
        return true
    }

    /// The result the keys rest on in the entry page. `nil` when they rest on the entry.
    private var focusedResult: PromptResult? {
        history.detail.flatMap { model.focusedResult(on: $0.id) }
    }

    /// A digit runs its prompt by dictation, or on the prompt input. With ⌘, it runs on the
    /// selected entry. Returns whether one ran.
    private func runNumber(_ event: NSEvent) -> Bool {
        guard model.renamingID == nil, let character = event.charactersIgnoringModifiers?.first,
            let number = character.wholeNumberValue, character.isASCII
        else { return false }
        switch event.modifierFlags.intersection(PaletteKey.chordModifiers) {
        case .command:
            guard let run = model.prompt(forNumber: number) else { return false }
            runPrompt(run.promptID, on: run.entry)
        case []:
            guard let command = model.command(forNumber: number) else { return false }
            run(command)
        default:
            return false
        }
        return true
    }

    /// ↓ and ↑ with no modifier, where no field takes them.
    private static func arrowStep(for event: NSEvent) -> Int? {
        guard event.modifierFlags.intersection(PaletteKey.chordModifiers).isEmpty else { return nil }
        switch Int(event.keyCode) {
        case kVK_DownArrow: return 1
        case kVK_UpArrow: return -1
        default: return nil
        }
    }

    private static func step(for event: NSEvent) -> Int? {
        if PaletteKey.next.matches(event) { return 1 }
        if PaletteKey.previous.matches(event) { return -1 }
        return nil
    }

    func shows(ticket: UInt64) -> Bool {
        promptRun.shows(ticket: ticket)
    }

    func deliverResult() {
        guard case .replied(let text) = promptRun.result?.state else { return }
        deliver(text)
    }

    /// The palette holds the keys, so it closes before the text is typed. Text
    /// too long to type keeps it open, to say so where the user asked.
    private func deliver(_ text: String) {
        guard ChatClient.fitsDelivery(text) else { return history.refuseTyping() }
        let application = frontApp?.bundleIdentifier
        close()
        Task {
            do {
                delivered(.success(try await ChatClient.deliver(text: text, application: application)))
            } catch {
                log.error("Typing into the app failed: \(String(describing: error), privacy: .private)")
                delivered(.failure(error))
            }
        }
    }

    /// Opens the draft once the daemon has read the file, unless the palette opened again since. A
    /// failed read says why on the row that asked.
    func editPrompt(_ id: String) {
        guard let entry = settings?.snapshot?.prompts.entries.first(where: { $0.id == id }),
            let row = model.selected?.id
        else { return }
        let opening = openings
        Task {
            do {
                let contents = try await ChatClient.getPrompt(id: id)
                guard opening == openings else { return }
                model.open(PromptDraft(editing: entry, contents: contents))
            } catch {
                log.error("Reading a prompt failed: \(String(describing: error), privacy: .private)")
                guard opening == openings else { return }
                model.failToEdit(on: row, PromptDraft.Failure(error))
            }
        }
    }

    /// Asks over the palette, so choosing doesn't close it. The file goes to
    /// the draft that asked, once the daemon has read it.
    func chooseFile() {
        guard let draft = model.draft?.identity else { return }
        Task {
            guard let file = await panel.choose(PromptFile.openPanel()) else { return }
            do {
                let contents = try await ChatClient.getPrompt(file: file)
                guard model.draft?.identity == draft else { return }
                model.draft?.choose(file, contents: contents)
            } catch {
                log.error("Reading a prompt file failed: \(String(describing: error), privacy: .private)")
                guard model.draft?.identity == draft else { return }
                model.draft?.fail(PromptDraft.Failure(error))
            }
        }
    }

    /// The daemon writes the prompt list, and the file when the instructions changed. The list
    /// then arrives with the settings, as any other writer's change does.
    func saveDraft() {
        guard let draft = model.draft, draft.problem == nil, savingDraft == nil else { return }
        let request = draft.saveRequest
        savingDraft = Task {
            defer { savingDraft = nil }
            model.finishSave(of: draft, with: await PromptChanges().save(request))
        }
    }

    func runPrompt(_ id: String, on input: HistoryItem) {
        history.forgetCopied()
        promptRun.start(.stored(id: id), named: model.promptName(id) ?? "Prompt", on: input)
        model.show(.result)
    }

    func stopTranscription() {
        Task { await listen.stop() }
    }

    func escape() {
        if model.page == .result { promptRun.cancel() }
        if model.escape() { close() }
    }

    private func close() {
        typing?.cancel()
        promptRun.release()
        model.closeMenu()
        deletion = nil
        panel.hide()
        history.close()
    }

    /// The palette stays open until the session is accepted, so a refusal
    /// can say so where the user asked.
    private func startTranscription() {
        let opening = openings
        Task {
            await listen.start()
            if !listen.didFailToStart, opening == openings { close() }
        }
    }

    func open() {
        openings += 1
        let front = NSWorkspace.shared.frontmostApplication
        frontApp = front?.bundleIdentifier == Bundle.main.bundleIdentifier ? nil : front
        model.reset(prompts: enabledPrompts, isListening: listen.isRunning)
        if model.keepsHistory { history.open() }
        panel.show(PaletteView(controller: self, listen: listen, history: history))
    }
}
