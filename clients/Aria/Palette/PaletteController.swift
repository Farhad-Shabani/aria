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
    /// The chat waiting on its delete to be confirmed.
    @Published var chatDeletion: ChatKey?
    /// Where Return types text: the app in front when the palette opened.
    private(set) var frontApp: NSRunningApplication?
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    let systemTranscription: SystemTranscriptionStatus
    let history: HistoryState
    let chats: Chats

    private let prompts: PromptHotkeys
    private let ask: PressController
    private let openSettings: @MainActor (SettingsPane?) -> Void
    private let openSetup: @MainActor () -> Void
    private let setup: SetupStatus
    private let settingsHealth: SettingsHealth
    /// Shows in the pill how typing went, once the palette is gone.
    private let delivered: @MainActor (Result<Aria_Daemon_V1_DeliveryOutcome, any Error>) -> Void
    private lazy var panel = PalettePanel(
        resignedKey: { [weak self] in self?.close() },
        pressed: { [weak self] event in self?.press(event) ?? false })
    private var gesture = PaletteGesture()
    private var chordChange: AnyCancellable?
    private var historySettingChange: AnyCancellable?
    private var assistantChange: AnyCancellable?
    private var systemTranscriptionChange: AnyCancellable?
    private var entriesChange: AnyCancellable?
    private var filterChange: AnyCancellable?
    private var pageChange: AnyCancellable?
    private var replyChange: AnyCancellable?
    private var promptsChange: AnyCancellable?
    private var setupChange: AnyCancellable?
    /// Where Edit Prompt finds a prompt. `nil` until the app tracks the settings.
    private var settings: DaemonSettings?
    /// The prompts that are on, in Settings order, as the settings last said.
    private var enabledPrompts: [Aria_Daemon_V1_Prompt] = []
    private var savingDraft: Task<Void, Never>?
    /// Fetches an entry's text to type. Closing the palette cancels it.
    private var typing: Task<Void, Never>?
    private var namingAssistants: Task<Void, Never>?
    /// Counts the times the palette opened, so work that outlives one opening
    /// leaves the next alone.
    private var openings = 0

    init(
        prompts: PromptHotkeys, ask: PressController, systemTranscription: SystemTranscriptionStatus,
        history: HistoryState, chats: Chats,
        events: DaemonEvents, setup: SetupStatus, settingsHealth: SettingsHealth,
        openSettings: @escaping @MainActor (SettingsPane?) -> Void,
        openSetup: @escaping @MainActor () -> Void,
        delivered: @escaping @MainActor (Result<Aria_Daemon_V1_DeliveryOutcome, any Error>) -> Void
    ) {
        self.prompts = prompts
        self.ask = ask
        self.systemTranscription = systemTranscription
        self.history = history
        self.chats = chats
        self.openSettings = openSettings
        self.openSetup = openSetup
        self.setup = setup
        self.settingsHealth = settingsHealth
        self.delivered = delivered
        promptRun = PaletteRun(
            effects: .init(
                run: { entry, source in try await ChatClient.runPromptForReply(entry: entry, source: source) },
                cancel: { ticket in try await ChatClient.cancel(ticket: ticket) }))
        events.onChat { [weak self] event in self?.promptRun.apply(event) }
        // A run's reply is saved under its entry, so the entry's count, its
        // open thread, and the chat ask saved as that entry catch up.
        replyChange =
            promptRun.$result
            .compactMap { result -> (ticket: UInt64?, entry: UInt64)? in
                guard let result, result.source != nil, case .replied = result.state else { return nil }
                return (result.ticket, result.input.id)
            }
            .removeDuplicates { $0.ticket == $1.ticket }
            .sink { [weak self] reply in
                guard let self else { return }
                history.refresh()
                reloadThread()
                chats.countResult(on: reply.entry)
            }
        pageChange =
            $model
            .map(\.page)
            .removeDuplicates()
            .sink { [weak self] _ in self?.history.forgetCopied() }
        systemTranscriptionChange = systemTranscription.$isRunning
            .removeDuplicates()
            .sink { [weak self] isRunning in self?.model.isSystemTranscriptionRunning = isRunning }
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
            .sink { [weak self] keepsHistory in
                self?.model.keepsHistory = keepsHistory
                self?.chats.keepsHistory = keepsHistory
            }
        assistantChange = settings.$assistantName
            .removeDuplicates()
            .sink { [weak self] name in self?.model.assistantName = name }
    }

    /// The app scene owns the poller, so it arrives after init.
    func track(_ poller: StatusPoller) {
        setupChange = Publishers.CombineLatest3(setup.$items, settingsHealth.$status, poller.$failure)
            .map { SetupItem.all(inspected: $0, settingsStatus: $1, startFailure: $2).count }
            .removeDuplicates()
            .sink { [weak self] steps in self?.model.replace(setupSteps: steps) }
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
            // Closes first, as the Ask shortcut does, so the question starts a chat and the
            // palette opens on it over the app in front.
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
        case .openSettings(let pane):
            closeForWindow()
            openSettings(pane)
        case .openSetup:
            closeForWindow()
            openSetup()
        }
    }

    func choose(_ item: PaletteItem) {
        switch item {
        case .command(let command):
            run(command)
        case .entry(let entry):
            openEntry(entry.id)
        case .chat(let chat):
            chats.open(chat.key, folder: chat.newest.project)
            showChat()
        case .result(let result):
            guard let entry = model.threadEntry else { return }
            history.forgetCopied()
            promptRun.show(saved: result, on: entry)
            model.show(.result)
        }
    }

    private func openEntry(_ id: UInt64) {
        history.select(id)
        model.open(entry: id)
        loadThread()
    }

    func openEntry(of ask: ChatAsk) {
        guard let id = ask.entryID else { return }
        openEntry(id)
    }

    var askTarget: AskTarget? {
        model.askTarget(shown: chats.shown)
    }

    /// Shows the chat `Chats.shown` holds.
    func showChat() {
        if !panel.isVisible { open() }
        if model.page == .result { promptRun.cancel() }
        model.openChat()
    }

    func returnOnChat() {
        switch decisions.chatReturn {
        case .send?:
            guard chats.send(model.followUp) else { return }
            model.followUp = ""
            // Back to resting, so the selection follows the reply to this follow-up.
            model.messageSelection = MessageSelection()
        case .type(let reply)?:
            deliver(reply)
        case nil:
            break
        }
    }

    var canReturnOnChat: Bool {
        switch decisions.chatReturn {
        case .send?: chats.canSend(model.followUp)
        case .type?: !history.isClearing
        case nil: false
        }
    }

    func deleteChat(_ chat: ChatKey) {
        history.deleteChat(chat)
        guard chats.shown?.key == chat else { return }
        chats.close()
        model.leaveChat()
    }

    /// Return: saved text is typed into the app in front, as in a clipboard
    /// history. Anything else, or with no app to type into, is chosen.
    func chooseSelected() {
        guard let item = model.selected else { return }
        guard decisions.typesOnReturn(item) else { return choose(item) }
        switch item {
        case .result(let result):
            deliver(result.text)
        case .entry, .chat:
            // A row holds only the start of its text.
            guard typing == nil, let entry = item.historyItem else { return }
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
        guard let renaming = model.renaming else { return }
        let title = model.renamingTitle
        model.stopRenaming()
        switch renaming {
        case .entry(let id):
            history.rename(id, to: title)
        case .chat(let key):
            history.renameChat(key, to: title) { [weak self] renamed in
                self?.chats.renamed(key, to: renamed)
            }
        }
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
            model: model, result: promptRun.result, isSystemTranscriptionRunning: systemTranscription.isRunning,
            isStopping: systemTranscription.isStopping,
            hasTranscript: !systemTranscription.transcript.lines.isEmpty, detail: history.detail,
            chat: chats.shown, history: history.work, hasFrontApp: frontApp != nil)
    }

    func perform(_ action: PaletteAction) {
        switch action.kind {
        case .copyEntry(let entry): history.copy(entry.id)
        case .copyDetail:
            if let result = focusedResult { history.copy(text: result.text) } else { history.copy() }
        case .copyText(let text): history.copy(text: text)
        case .copyTranscript: history.copy(text: systemTranscription.transcript.text)
        case .runPrompt(let entry): model.attach(entry)
        case .transcribeAgain: history.regenerate()
        case .rename(let entry): model.startRenaming(entry)
        case .renameChat(let key): model.startRenaming(key)
        case .delete(let entry): deletion = entry
        case .open(let item): choose(item)
        case .editPrompt(let id): editPrompt(id)
        case .saveAsPrompt(let instructions):
            model.open(PromptDraft(instructions: instructions, kind: .compose), replacing: true)
        case .retry: promptRun.retry()
        case .stop: stopTranscription()
        case .typeText(let text): deliver(text)
        case .stopAsk: chats.stop()
        case .deleteChat(let chat): chatDeletion = chat
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
        if PaletteKey.copy.matches(event), panel.hasSelection { return false }
        if model.page == .prompt, PaletteKey.save.matches(event) {
            saveDraft()
            return true
        }
        // While a name is edited, the keys go to its field.
        if model.page == .chat, !model.isRenaming {
            let asks = chats.shown?.asks ?? []
            if let step = Self.step(for: event) ?? Self.arrowStep(for: event) {
                model.messageSelection.move(by: step, extending: false, in: asks)
                return true
            }
            if let step = Self.extendStep(for: event) {
                model.messageSelection.move(by: step, extending: true, in: asks)
                return true
            }
            // With a follow-up typed, ⌘A selects its text instead.
            if PaletteKey.selectAll.matches(event), model.followUp.isEmpty {
                model.messageSelection.selectAll(in: asks)
                return true
            }
            // As in Messages: with something typed, ⌘⌫ deletes it rather than the chat.
            if PaletteKey.delete.matches(event), !model.followUp.isEmpty { return false }
        }
        if model.page == .entry, let entry = history.detail?.id,
            let step = Self.step(for: event) ?? Self.arrowStep(for: event)
        {
            // The ends of the thread still take the keys, so they don't beep.
            model.moveDetailFocus(by: step, on: entry)
            return !model.isRenaming
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
        guard !model.isRenaming, let character = event.charactersIgnoringModifiers?.first,
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

    private static func extendStep(for event: NSEvent) -> Int? {
        guard event.modifierFlags.intersection(PaletteKey.chordModifiers) == .shift else { return nil }
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
        promptRun.shows(ticket: ticket) || chats.shows(ticket: ticket)
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
        Task { await systemTranscription.stop() }
    }

    func escape() {
        // Several messages selected go back to one before the page goes.
        if model.page == .chat, model.messageSelection.collapse(in: chats.shown?.asks ?? []) { return }
        let leavesChat = model.page == .chat
        if model.page == .result { promptRun.cancel() }
        if model.escape() { close() }
        if !model.returnsToChat { chats.close() }
        // The list shows a chat at its newest ask, which may have come since it was read.
        if leavesChat, model.page == .commands, history.isOpen { history.refresh() }
    }

    /// The palette's panel never activates Aria, and macOS declines a plain
    /// activate() while another app is in front. The window would open behind that app.
    private func closeForWindow() {
        NSApplication.shared.activate(ignoringOtherApps: true)
        close()
    }

    private func close() {
        typing?.cancel()
        promptRun.release()
        model.closeMenu()
        deletion = nil
        chatDeletion = nil
        chats.close()
        panel.hide()
        history.close()
    }

    /// The palette stays open until the session is accepted, so a refusal
    /// can say so where the user asked.
    private func startTranscription() {
        let opening = openings
        Task {
            await systemTranscription.start()
            if !systemTranscription.didFailToStart, opening == openings { close() }
        }
    }

    func open() {
        openings += 1
        let front = NSWorkspace.shared.frontmostApplication
        frontApp = front?.bundleIdentifier == Bundle.main.bundleIdentifier ? nil : front
        model.reset(prompts: enabledPrompts, isSystemTranscriptionRunning: systemTranscription.isRunning)
        // A step may be done or undone outside Aria.
        let setup = setup
        Task { await setup.refresh() }
        if model.keepsHistory { history.open() }
        nameAssistants()
        panel.show(PaletteView(controller: self, systemTranscription: systemTranscription, history: history))
    }

    /// Until the daemon gives the assistants' names, a chat names its folder.
    private func nameAssistants() {
        guard model.assistantNames.isEmpty, namingAssistants == nil else { return }
        namingAssistants = Task {
            defer { namingAssistants = nil }
            do {
                let assistants = try await ChatClient.assistants()
                model.assistantNames = Dictionary(assistants.map { ($0.id, $0.name) }) { first, _ in first }
            } catch {
                log.error("Naming the assistants failed: \(String(describing: error), privacy: .private)")
            }
        }
    }

    func assistantName(of chat: ChatKey?) -> String? {
        chat.flatMap { model.assistantNames[$0.assistantID] }
    }
}
