import AppKit

/// What the palette shows and which row is selected, decided apart from the
/// panel and SwiftUI.
struct PaletteModel: Equatable {
    /// One entry's prompt results, newest first. Its page reads them, and
    /// while open they show as rows under it.
    struct Thread: Equatable {
        let entryID: UInt64
        var results: [PromptResult]
        var isOpen: Bool
    }

    /// The entry whose results are being read, and whether they open as rows.
    struct ThreadRequest: Equatable {
        let entryID: UInt64
        let opens: Bool
    }

    static let numberKeyCount = 9

    /// The ⌘K list belongs to the page, row and search it opened on.
    private(set) var page = PalettePage.commands {
        didSet { menuIndex = nil }
    }
    /// Typing moves the selection back to the first match. A field ending its
    /// edit writes back the same text, which leaves the selection be.
    var query = "" {
        didSet {
            guard query != oldValue else { return }
            selectedIndex = 0
            menuIndex = nil
            dropThread()
        }
    }
    /// A running session pins its row first, where the selection starts.
    var isSystemTranscriptionRunning = false {
        didSet {
            selectedIndex = 0
            menuIndex = nil
        }
    }
    /// While off, no saved entry or history command shows.
    var keepsHistory = false
    var isConfirmingClear = false
    /// The entry whose title is being edited in place. The selection holds
    /// still meanwhile.
    private(set) var renamingID: UInt64?
    var renamingTitle = ""
    /// The one history section the list keeps to. `nil` shows every section.
    private(set) var section: HistorySection?
    /// The entry a prompt runs on, shown as a chip in the search field. The
    /// list holds only prompts meanwhile.
    private(set) var input: HistoryItem?
    private(set) var selectedIndex = 0 {
        didSet { if selectedIndex != oldValue { editFailure = nil } }
    }
    /// The row of the ⌘K list that Return runs. `nil` while the list is closed.
    private(set) var menuIndex: Int?
    /// The one thread held. A search, a section or a prompt input drops it.
    private(set) var thread: Thread?
    /// The thread still being read. Results that don't match it came too late.
    private(set) var threadRequest: ThreadRequest?
    /// The result the keys rest on in an open entry's page. `nil` rests on the entry itself.
    private(set) var focusedResultID: UInt64?
    /// The prompt being written or edited. Closing the palette keeps it, for
    /// New Prompt or Edit Prompt to bring back.
    var draft: PromptDraft?
    var isConfirmingDiscard = false
    /// A draft opened over a changed one. It shows once the changed one is discarded.
    private var waitingDraft: PromptDraft?
    /// Why Edit Prompt couldn't read the file, and the row it was asked on.
    private var editFailure: EditFailure?
    /// New Prompt shows while the prompts are under the daemon's limit.
    var canAddPrompt = false
    var assistantName: String?
    /// The steps left in Finish Setting Up Aria.
    private(set) var setupSteps = 0
    private var prompts: [PaletteCommand] = []
    /// Already filtered by the query, newest day first.
    private var entries: [HistoryItem] = []

    private struct EditFailure: Equatable {
        let failure: PromptDraft.Failure
        let rowID: PaletteItem.ID
    }

    /// Starts afresh each time the palette opens.
    mutating func reset(prompts: [Aria_Daemon_V1_Prompt], isSystemTranscriptionRunning: Bool) {
        self.prompts = Self.commands(for: prompts)
        self.isSystemTranscriptionRunning = isSystemTranscriptionRunning
        page = .commands
        query = ""
        section = nil
        isConfirmingClear = false
        isConfirmingDiscard = false
        waitingDraft = nil
        editFailure = nil
        renamingID = nil
        input = nil
        dropThread()
    }

    /// Follows settings while the palette is open. The selection stays on its row.
    mutating func replace(prompts: [Aria_Daemon_V1_Prompt]) {
        let selectedID = selected?.id
        self.prompts = Self.commands(for: prompts)
        reselect(selectedID)
    }

    /// Follows setup while the palette is open. The selection stays on its row.
    mutating func replace(setupSteps: Int) {
        let selectedID = selected?.id
        self.setupSteps = setupSteps
        reselect(selectedID)
    }

    private mutating func reselect(_ id: PaletteItem.ID?) {
        selectedIndex = items.firstIndex { $0.id == id } ?? min(selectedIndex, max(items.count - 1, 0))
        menuIndex = nil
    }

    /// Shows `draft`, unless a kept draft for the same prompt exists and `replacing` is off. A kept
    /// draft with changes is discarded only once the user says so.
    mutating func open(_ draft: PromptDraft, replacing: Bool = false) {
        show(.prompt)
        guard let kept = self.draft else {
            self.draft = draft
            return
        }
        if !replacing && kept.id == draft.id { return }
        if kept.hasChanges {
            waitingDraft = draft
            isConfirmingDiscard = true
        } else {
            self.draft = draft
        }
    }

    /// Drops the draft without saving. Shows the one waiting on it, if any,
    /// or else leaves the prompt page.
    mutating func discardDraft() {
        isConfirmingDiscard = false
        draft = waitingDraft
        waitingDraft = nil
        if draft == nil { page = .commands }
    }

    mutating func keepDraft() {
        isConfirmingDiscard = false
        waitingDraft = nil
    }

    /// Why Edit Prompt didn't open, while its row stays selected on the
    /// commands page.
    var promptFailure: PromptDraft.Failure? {
        guard let editFailure, page == .commands, selected?.id == editFailure.rowID else { return nil }
        return editFailure.failure
    }

    mutating func failToEdit(on rowID: PaletteItem.ID, _ failure: PromptDraft.Failure) {
        editFailure = EditFailure(failure: failure, rowID: rowID)
    }

    /// Applies how the save of `saved` ended. An unchanged draft closes. One
    /// changed meanwhile stays, now editing the prompt saved. A draft opened
    /// in its place is left alone.
    mutating func finishSave(
        of saved: PromptDraft, with outcome: Result<Aria_Daemon_V1_SavePromptResponse, PromptDraft.Failure>
    ) {
        guard draft?.identity == saved.identity else { return }
        switch outcome {
        case .success where draft == saved: discardDraft()
        case .success(let response): draft?.didSave(saved, as: response)
        case .failure(let failure): draft?.fail(failure)
        }
    }

    /// With no prompts there is nothing to run on an entry.
    var canAttach: Bool { !prompts.isEmpty }

    mutating func attach(_ entry: HistoryItem) {
        guard canAttach else { return }
        input = entry
        dropThread()
        page = .commands
        query = ""
        selectedIndex = 0
    }

    func promptName(_ id: String) -> String? {
        prompts.first { $0.action == .runPrompt(id: id) }?.title
    }

    mutating func startRenaming(_ entry: HistoryItem) {
        renamingID = entry.id
        renamingTitle = entry.title ?? ""
    }

    mutating func stopRenaming() {
        renamingID = nil
    }

    var commands: [PaletteCommand] {
        guard input == nil else {
            let instructions = query.trimmingCharacters(in: .whitespacesAndNewlines)
            return matching(prompts) + (instructions.isEmpty ? [] : [.oneOff(instructions)])
        }
        return matching(pageCommands)
    }

    /// Every command the commands page offers with no prompt input, before search.
    private var pageCommands: [PaletteCommand] {
        [isSystemTranscriptionRunning ? .liveTranscript : .newTranscription, .newDictation, .ask(assistantName)]
            + prompts
            + (canAddPrompt ? [.newPrompt] : []) + [.setUp, .settings] + (setupSteps > 0 ? [.finishSetup] : [])
            + (keepsHistory ? HistorySection.allCases.map(PaletteCommand.history) + [.clearHistory] : [])
    }

    private func matching(_ commands: [PaletteCommand]) -> [PaletteCommand] {
        let needle = query.trimmingCharacters(in: .whitespaces)
        guard !needle.isEmpty else { return commands }
        return commands.filter { command in
            ([command.title] + command.keywords).contains { $0.localizedStandardContains(needle) }
        }
    }

    /// With nothing typed, history entries take the commands' place: history
    /// loads page by page, so commands after it would never come into view.
    var items: [PaletteItem] {
        guard input == nil else { return commands.map(PaletteItem.command) }
        let entries = keepsHistory ? threaded(entries) : []
        guard section == nil else { return entries }
        guard !isShowingHistoryFirst else {
            return (isSystemTranscriptionRunning ? [.command(.liveTranscript)] : []) + entries
        }
        return commands.map(PaletteItem.command) + entries
    }

    private func threaded(_ entries: [HistoryItem]) -> [PaletteItem] {
        entries.flatMap { entry -> [PaletteItem] in
            guard let thread, thread.isOpen, thread.entryID == entry.id else { return [.entry(entry)] }
            return [.entry(entry)] + thread.results.map(PaletteItem.result)
        }
    }

    /// The entry whose thread the selection opens or folds: the selected
    /// entry, when it has results, or the entry of a selected result.
    var threadEntry: HistoryItem? {
        switch selected {
        case .entry(let entry) where entry.resultCount > 0: entry
        case .result: thread.flatMap { thread in entries.first { $0.id == thread.entryID } }
        case .entry, .command, nil: nil
        }
    }

    func isThreadOpen(of entry: UInt64) -> Bool {
        thread?.entryID == entry && thread?.isOpen == true
    }

    /// Asks for `entry`'s results, replacing any request still out.
    mutating func requestThread(of entry: UInt64, opens: Bool) {
        threadRequest = ThreadRequest(entryID: entry, opens: opens)
    }

    /// Holds `entry`'s results if they are the ones asked for. They open as
    /// rows when asked to, or when they already were. The selection stays on
    /// its row.
    mutating func replace(results: [PromptResult], of entry: UInt64) {
        guard let request = threadRequest, request.entryID == entry else { return }
        let selectedID = selected?.id
        thread = Thread(entryID: entry, results: results, isOpen: request.opens || isThreadOpen(of: entry))
        threadRequest = nil
        menuIndex = nil
        selectedIndex = items.firstIndex { $0.id == selectedID } ?? selectedIndex
    }

    /// Folds the open thread, or one still opening. A selection inside it
    /// moves to its entry. Returns whether a thread was open.
    @discardableResult
    mutating func foldThread() -> Bool {
        if threadRequest?.opens == true { threadRequest = nil }
        guard let thread, thread.isOpen else { return false }
        let isInThread = selected?.result != nil
        self.thread?.isOpen = false
        menuIndex = nil
        if isInThread, let index = items.firstIndex(where: { $0.entry?.id == thread.entryID }) {
            selectedIndex = index
        }
        return true
    }

    private mutating func dropThread() {
        thread = nil
        threadRequest = nil
    }

    /// `entry`'s results, oldest first, as its page reads them.
    func threadResults(of entry: UInt64) -> [PromptResult] {
        guard let thread, thread.entryID == entry else { return [] }
        return thread.results.reversed()
    }

    /// The result the keys rest on, while it is still in `entry`'s thread.
    func focusedResult(on entry: UInt64) -> PromptResult? {
        threadResults(of: entry).first { $0.id == focusedResultID }
    }

    /// While true, the list hides the commands until something is typed. The digits and ⌘, still
    /// run theirs.
    var isShowingHistoryFirst: Bool {
        keepsHistory && section == nil && !entries.isEmpty && query.trimmingCharacters(in: .whitespaces).isEmpty
    }

    var selected: PaletteItem? {
        let items = items
        return items.indices.contains(selectedIndex) ? items[selectedIndex] : nil
    }

    /// A selection on the top row stays on top, so the newest entry is the one
    /// selected once history loads. Any other selection follows its row.
    mutating func replace(entries: [HistoryItem]) {
        let selectedID = selected?.id
        self.entries = entries
        if let thread, !entries.contains(where: { $0.id == thread.entryID }) {
            dropThread()
        }
        guard selectedIndex > 0 else { return }
        let items = items
        if let index = items.firstIndex(where: { $0.id == selectedID }) {
            selectedIndex = index
        } else {
            selectedIndex = min(selectedIndex, max(items.count - 1, 0))
        }
    }

    /// The day to title above the row at `index`, when it starts one.
    func dayStarting(at index: Int, calendar: Calendar = .current) -> Date? {
        Self.dayStarting(at: index, in: items, calendar: calendar)
    }

    /// Takes `items` so a list can build them once per render, not once per row.
    static func dayStarting(at index: Int, in items: [PaletteItem], calendar: Calendar = .current) -> Date? {
        guard let entry = items[index].entry else { return nil }
        let day = calendar.startOfDay(for: entry.created)
        let previous = items[..<index].last { $0.entry != nil }?.entry
        guard let previous, calendar.startOfDay(for: previous.created) == day else { return day }
        return nil
    }

    mutating func select(_ id: PaletteItem.ID) {
        guard renamingID == nil, let index = items.firstIndex(where: { $0.id == id }), index != selectedIndex
        else { return }
        selectedIndex = index
        menuIndex = nil
    }

    mutating func moveSelection(by step: Int) {
        let last = items.count - 1
        guard renamingID == nil, last >= 0 else { return }
        selectedIndex = min(max(selectedIndex + step, 0), last)
        menuIndex = nil
    }

    mutating func toggleMenu(count: Int) {
        menuIndex = menuIndex == nil && count > 0 ? 0 : nil
    }

    mutating func moveMenu(by step: Int, count: Int) {
        guard let index = menuIndex else { return }
        menuIndex = min(max(index + step, 0), max(count - 1, 0))
    }

    mutating func selectMenu(at index: Int) {
        guard menuIndex != nil else { return }
        menuIndex = index
    }

    mutating func closeMenu() {
        menuIndex = nil
    }

    /// A digit runs a prompt only on the commands page with nothing typed, so
    /// digits can still be searched for.
    func command(forNumber number: Int) -> PaletteCommand? {
        guard page == .commands, input != nil || section == nil, query.isEmpty else { return nil }
        return prompts.first { $0.number == number }
    }

    /// ⌘ and a digit: that prompt, run on the selected entry, whatever is typed.
    func prompt(forNumber number: Int) -> (promptID: String, entry: HistoryItem)? {
        guard page == .commands, renamingID == nil, input == nil, case .entry(let entry) = selected,
            case .runPrompt(let id)? = prompts.first(where: { $0.number == number })?.action
        else { return nil }
        return (id, entry)
    }

    /// A command's key runs it from the commands page, whatever is typed, while the page offers it.
    func command(pressing event: NSEvent) -> PaletteCommand? {
        guard page == .commands, renamingID == nil, input == nil else { return nil }
        return pageCommands.first { $0.key?.matches(event) == true }
    }

    /// The pills pick a section while history shows and no prompt input does.
    var showsSectionPills: Bool {
        keepsHistory && input == nil
    }

    /// Moves to the next section pill, or the previous one, wrapping round
    /// past All. Returns whether the pills could move.
    mutating func cycleSection(by step: Int) -> Bool {
        guard page == .commands, renamingID == nil, showsSectionPills else { return false }
        let order: [HistorySection?] = [nil] + HistorySection.allCases
        let index = order.firstIndex(of: section) ?? 0
        show(section: order[(index + step + order.count) % order.count])
        return true
    }

    /// Keeps the list to one section, or to none, and clears the search.
    mutating func show(section: HistorySection?) {
        self.section = section
        dropThread()
        query = ""
    }

    mutating func show(_ page: PalettePage) {
        self.page = page
        renamingID = nil
    }

    /// Opens `entry`'s page on its own text, and asks for its thread, which
    /// the page reads without opening it in the list.
    mutating func open(entry: UInt64) {
        show(.entry)
        focusedResultID = nil
        requestThread(of: entry, opens: false)
    }

    /// Steps from `entry` through its results, stopping at either end.
    mutating func moveDetailFocus(by step: Int, on entry: UInt64) {
        guard renamingID == nil else { return }
        let order = [nil] + threadResults(of: entry).map { Optional($0.id) }
        let index = order.firstIndex(of: focusedResultID) ?? 0
        focusedResultID = order[min(max(index + step, 0), order.count - 1)]
    }

    /// Escape ends a rename, asks before dropping a changed draft, or leaves a page. On the
    /// commands page it folds the thread, clears what is typed, drops the prompt input, then widens
    /// the section. Returns whether the palette closes.
    mutating func escape() -> Bool {
        if renamingID != nil {
            renamingID = nil
            return false
        }
        if page == .prompt {
            if draft?.hasChanges == true {
                waitingDraft = nil
                isConfirmingDiscard = true
            } else {
                discardDraft()
            }
            return false
        }
        if page != .commands {
            page = .commands
            return false
        }
        if foldThread() { return false }
        guard query.isEmpty else {
            query = ""
            return false
        }
        if let input {
            self.input = nil
            let row = PaletteItem.entry(input).id
            selectedIndex = items.firstIndex { $0.id == row } ?? 0
            return false
        }
        guard section == nil else {
            section = nil
            return false
        }
        return true
    }

    private static func commands(for prompts: [Aria_Daemon_V1_Prompt]) -> [PaletteCommand] {
        prompts.enumerated().map { index, prompt in
            PaletteCommand(
                id: prompt.id,
                title: prompt.name,
                symbol: "text.bubble",
                number: index < numberKeyCount ? index + 1 : nil,
                chord: prompt.storedChord,
                action: .runPrompt(id: prompt.id)
            )
        }
    }
}
