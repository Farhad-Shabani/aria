import SwiftUI
import XCTest

@testable import Aria

final class PaletteTests: XCTestCase {
    private let prompts = (1...10).map { Aria_Daemon_V1_Prompt.stub(file: "/\($0).md") }

    private static func entry(_ id: UInt64, at seconds: TimeInterval) -> HistoryItem {
        HistoryItem(id: id, created: Date(timeIntervalSince1970: seconds), text: "Text \(id)", status: .success)
    }

    private func openModel(isListening: Bool = false, history: [HistoryItem]? = nil) -> PaletteModel {
        var model = PaletteModel()
        model.reset(prompts: prompts, isListening: isListening)
        if let history {
            model.keepsHistory = true
            model.replace(entries: history)
        }
        return model
    }

    func testThePaletteChordIsIgnoredWhileItsRunGoesAndOtherwiseToggles() {
        var gesture = PaletteGesture()
        XCTAssertEqual(gesture.press(isOpen: false) { _ in true }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)

        gesture.chose("started")
        XCTAssertEqual(gesture.press(isOpen: false) { $0 == "started" }, .ignore)
        XCTAssertEqual(gesture.press(isOpen: false) { _ in false }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)
    }

    func testOnlyTheFirstNinePromptsGetANumberAndSettingsComesLast() {
        let rows = openModel().commands

        XCTAssertEqual(rows.compactMap(\.number), Array(1...9))
        XCTAssertNil(rows.first { $0.id == prompts[9].id }?.number)
        XCTAssertEqual(rows.last?.action, .openSettings)
    }

    func testNewDictationAndAskFollowTranscriptionAndClearHistoryShowsOnlyWithHistoryOn() {
        var model = openModel()
        XCTAssertEqual(model.commands.prefix(3).map(\.action), [.startTranscription, .startDictation, .startAsk])
        XCTAssertFalse(model.commands.contains { $0.action == .clearHistory })

        model.keepsHistory = true

        XCTAssertEqual(model.commands.last?.action, .clearHistory)
    }

    func testASectionShowsOnlyItsEntriesAndEscapeWidensItBeforeClosing() {
        var model = openModel(isListening: true, history: [Self.entry(1, at: 100)])
        model.show(section: .dictation)

        XCTAssertEqual(model.items.map(\.id), ["entry-1"], "no commands, no live row")
        XCTAssertNil(model.command(forNumber: 1))
        model.query = "x"
        XCTAssertFalse(model.escape(), "the search clears first")
        XCTAssertEqual(model.section, .dictation)
        XCTAssertFalse(model.escape(), "then the section widens")
        XCTAssertNil(model.section)
        XCTAssertTrue(model.escape())
    }

    func testHistoryOnOffersACommandForEachSection() {
        var model = openModel()
        model.keepsHistory = true

        let sections = model.commands.compactMap { command -> HistorySection? in
            if case .showHistory(let section) = command.action { section } else { nil }
        }
        XCTAssertEqual(sections, [.dictation, .listen, .ask])
    }

    func testARenameHoldsTheSelectionAndEscapeEndsItFirst() {
        var model = openModel()
        model.keepsHistory = true
        var named = Self.entry(1, at: 100)
        named.title = "Standup"
        model.replace(entries: [Self.entry(2, at: 200), named])
        model.select("entry-1")

        model.startRenaming(named)
        model.moveSelection(by: -1)
        model.select("entry-2")

        XCTAssertEqual(model.renamingTitle, "Standup")
        XCTAssertEqual(model.selected?.entry?.id, 1)
        XCTAssertFalse(model.escape())
        XCTAssertNil(model.renamingID)
        XCTAssertEqual(model.selected?.entry?.id, 1, "the first Esc only ends the rename")
    }

    func testAPromptInputListsOnlyPromptsAndLetsDigitsRunThem() {
        var model = openModel(history: [Self.entry(2, at: 200), Self.entry(1, at: 100)])
        model.show(section: .dictation)

        model.attach(Self.entry(1, at: 100))

        XCTAssertEqual(model.items.compactMap(\.command?.action), prompts.map { .runPrompt(id: $0.id) })
        XCTAssertEqual(model.command(forNumber: 2)?.action, .runPrompt(id: prompts[1].id))
        model.query = "nothing like it"
        XCTAssertEqual(model.items.compactMap(\.command?.action), [.runOneOff("nothing like it")])
    }

    func testEscapeDropsThePromptInputAndReselectsItsRowBeforeWideningTheSection() {
        var model = openModel(history: [Self.entry(2, at: 200), Self.entry(1, at: 100)])
        model.show(section: .dictation)
        model.attach(Self.entry(1, at: 100))
        model.show(.result)

        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands)
        XCTAssertNotNil(model.input, "the result page goes back to the prompts")
        XCTAssertFalse(model.escape())
        XCTAssertNil(model.input)
        XCTAssertEqual(model.selected?.entry?.id, 1)
        XCTAssertEqual(model.section, .dictation)
    }

    func testCommandDigitRunsThatPromptOnTheSelectedEntryWhateverIsTyped() {
        var model = openModel()
        XCTAssertNil(model.prompt(forNumber: 2), "a command row has nothing to run on")
        model.keepsHistory = true
        model.replace(entries: [Self.entry(1, at: 100)])

        model.query = "Text"
        model.select("entry-1")

        XCTAssertEqual(model.prompt(forNumber: 2)?.promptID, prompts[1].id)
        XCTAssertEqual(model.prompt(forNumber: 2)?.entry.id, 1)
        XCTAssertNil(model.prompt(forNumber: 10), "only nine prompts have a number")
    }

    private static func key(_ characters: String, _ modifiers: NSEvent.ModifierFlags, code: UInt16 = 0) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0, windowNumber: 0, context: nil,
            characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code)!
    }

    func testAnActionKeyMatchesItsChordWithCapsLockOnButNotWithAnotherModifier() {
        XCTAssertTrue(PaletteKey.copy.matches(Self.key("c", [.command, .capsLock])))
        XCTAssertTrue(PaletteKey.delete.matches(Self.key("\u{7F}", .command)))
        XCTAssertTrue(PaletteKey.runPrompt.matches(Self.key("\t", [])))
        XCTAssertFalse(PaletteKey.copy.matches(Self.key("c", [.command, .shift])))
        XCTAssertFalse(PaletteKey.copy.matches(Self.key("c", [])), "typing c searches")
        XCTAssertFalse(PaletteKey.runPrompt.matches(Self.key("\t", .shift)))
        XCTAssertTrue(PaletteKey.next.matches(Self.key("n", [.control, .capsLock])))
    }

    private static let utc: Calendar = {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .gmt
        return calendar
    }()

    private static func threaded(_ id: UInt64, at seconds: TimeInterval, results: Int) -> HistoryItem {
        var entry = entry(id, at: seconds)
        entry.resultCount = results
        return entry
    }

    private static func result(_ id: UInt64) -> PromptResult {
        PromptResult(id: id, created: Date(timeIntervalSince1970: 0), promptName: "Summarize", text: "Short.")
    }

    private func modelWithThread() -> PaletteModel {
        var model = openModel(history: [Self.threaded(2, at: 200, results: 2), Self.entry(1, at: 100)])
        model.requestThread(of: 2, opens: true)
        model.replace(results: [Self.result(8), Self.result(7)], of: 2)
        return model
    }

    func testAnOpenThreadListsItsResultsRightUnderItsEntry() {
        var model = openModel(history: [Self.threaded(2, at: 200, results: 2), Self.entry(1, at: 100)])
        model.requestThread(of: 2, opens: true)
        model.replace(results: [Self.result(8), Self.result(7)], of: 2)

        XCTAssertEqual(model.items.map(\.id), ["entry-2", "result-8", "result-7", "entry-1"])
        XCTAssertEqual(model.selected?.id, "entry-2")
        XCTAssertNil(model.dayStarting(at: 3, calendar: Self.utc), "results don't break a day")
    }

    func testEscapeFoldsTheThreadFirstAndReselectsItsEntry() {
        var model = modelWithThread()
        model.moveSelection(by: 2)
        XCTAssertEqual(model.threadEntry?.id, 2, "a result opens or folds its own entry's thread")

        XCTAssertFalse(model.escape())

        XCTAssertFalse(model.isThreadOpen(of: 2))
        XCTAssertEqual(model.selected?.id, "entry-2")
    }

    func testTypingOrPickingASectionFoldsTheThread() {
        var model = modelWithThread()
        model.query = "x"
        XCTAssertNil(model.thread)

        model = modelWithThread()
        model.show(section: .dictation)
        XCTAssertNil(model.thread)
    }

    func testOnlyAnEntryWithResultsHasAThreadToOpen() {
        var model = modelWithThread()
        model.foldThread()
        XCTAssertEqual(model.threadEntry?.id, 2)

        model.moveSelection(by: 1)

        XCTAssertNil(model.threadEntry)
    }

    func testTheSectionPillsStepRoundAndHideWithoutHistory() {
        var model = openModel()
        XCTAssertFalse(model.cycleSection(by: 1), "no pills while history is off")
        XCTAssertNil(model.section)

        model.keepsHistory = true
        XCTAssertTrue(model.cycleSection(by: 1))
        XCTAssertEqual(model.section, .dictation)
        XCTAssertTrue(model.cycleSection(by: -2))
        XCTAssertEqual(model.section, .ask, "back past All wraps round")

        model.startRenaming(Self.entry(1, at: 100))
        XCTAssertFalse(model.cycleSection(by: 1), "a rename keeps the key")
    }

    func testAnOpenEntrysKeysStepThroughItsThreadOldestFirst() {
        var model = modelWithThread()
        model.open(entry: 2)
        XCTAssertEqual(model.threadResults(of: 2).map(\.id), [7, 8])
        model.moveDetailFocus(by: -1, on: 2)
        XCTAssertNil(model.focusedResult(on: 2), "the entry is the first block")
        model.moveDetailFocus(by: 5, on: 2)
        XCTAssertEqual(model.focusedResult(on: 2)?.id, 8, "stops at the last result")
        XCTAssertNil(model.focusedResult(on: 1), "another entry's page rests on its own text")

        model.startRenaming(Self.entry(2, at: 200))
        model.moveDetailFocus(by: -1, on: 2)
        XCTAssertEqual(model.focusedResult(on: 2)?.id, 8, "a rename keeps the arrows")
        model.stopRenaming()

        model.show(.commands)
        model.open(entry: 2)

        XCTAssertNil(model.focusedResult(on: 2))
    }

    func testOpeningAnEntrysPageReadsItsThreadWithoutOpeningItInTheList() {
        var model = openModel(history: [Self.threaded(2, at: 200, results: 2), Self.entry(1, at: 100)])

        model.open(entry: 2)
        XCTAssertEqual(model.threadRequest, .init(entryID: 2, opens: false))
        model.replace(results: [Self.result(8), Self.result(7)], of: 2)

        XCTAssertEqual(model.threadResults(of: 2).map(\.id), [7, 8])
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.items.map(\.id), ["entry-2", "entry-1"], "back to a folded list")
    }

    func testAThreadReadAfterALaterRequestIsDropped() {
        var model = openModel(history: [Self.threaded(2, at: 200, results: 2), Self.threaded(1, at: 100, results: 1)])

        model.requestThread(of: 2, opens: true)
        model.requestThread(of: 1, opens: false)
        model.replace(results: [Self.result(8)], of: 2)

        XCTAssertNil(model.thread)
        XCTAssertEqual(model.threadRequest?.entryID, 1)
    }

    func testFoldingAThreadStillOpeningKeepsItFolded() {
        var model = openModel(history: [Self.threaded(2, at: 200, results: 2)])

        model.requestThread(of: 2, opens: true)
        XCTAssertFalse(model.foldThread())
        model.replace(results: [Self.result(8)], of: 2)

        XCTAssertFalse(model.isThreadOpen(of: 2))
    }

    func testWithNoPromptsAnEntryCannotBeAttached() {
        var model = PaletteModel()
        model.reset(prompts: [], isListening: false)

        model.attach(Self.entry(1, at: 100))

        XCTAssertFalse(model.canAttach)
        XCTAssertNil(model.input)
    }

    func testTheActionsListOpensOnlyWithActionsAndStopsAtItsEnds() {
        var model = openModel()
        model.toggleMenu(count: 0)
        XCTAssertNil(model.menuIndex)

        model.toggleMenu(count: 3)
        model.moveMenu(by: -1, count: 3)
        XCTAssertEqual(model.menuIndex, 0)
        model.moveMenu(by: 5, count: 3)
        XCTAssertEqual(model.menuIndex, 2)
        model.toggleMenu(count: 3)
        XCTAssertNil(model.menuIndex)
    }

    func testTheActionsListClosesWhenTheSelectionPageOrSearchChanges() {
        var model = openModel()
        model.toggleMenu(count: 3)
        model.moveSelection(by: 1)
        XCTAssertNil(model.menuIndex)

        model.toggleMenu(count: 3)
        model.query = "x"
        XCTAssertNil(model.menuIndex)

        model.toggleMenu(count: 3)
        model.show(.liveTranscript)
        XCTAssertNil(model.menuIndex)
    }

    func testANewListOfEntriesKeepsTheActionsListOpen() {
        var model = openModel(history: [Self.entry(1, at: 100)])
        model.toggleMenu(count: 3)

        model.replace(entries: [Self.entry(2, at: 200), Self.entry(1, at: 100)])

        XCTAssertEqual(model.menuIndex, 0)
    }

    func testOpeningDropsAConfirmationLeftOpen() {
        var model = openModel()
        model.isConfirmingClear = true

        model.reset(prompts: prompts, isListening: false)

        XCTAssertFalse(model.isConfirmingClear)
    }

    func testARunningSessionIsPinnedFirstAndSelectedInPlaceOfNewTranscription() {
        var model = openModel()
        XCTAssertEqual(model.commands.first?.action, .startTranscription)

        model.moveSelection(by: 2)
        model.isListening = true

        XCTAssertEqual(model.selected?.command?.action, .showLiveTranscript)
        XCTAssertFalse(model.commands.contains { $0.action == .startTranscription })
    }

    func testADigitRunsItsPromptOnlyOnTheCommandsPageWithNothingTyped() {
        var model = openModel(isListening: true)
        XCTAssertEqual(model.command(forNumber: 2)?.action, .runPrompt(id: prompts[1].id))

        model.query = "s"
        XCTAssertNil(model.command(forNumber: 2))

        model.query = ""
        model.show(.liveTranscript)
        XCTAssertNil(model.command(forNumber: 2))
    }

    func testTypingFiltersTheRowsAndSelectsTheFirstMatch() {
        var model = openModel()
        model.moveSelection(by: 3)

        model.query = "sett"

        XCTAssertEqual(model.commands.map(\.action), [.openSettings])
        XCTAssertEqual(model.selected?.command?.action, .openSettings)
    }

    func testWritingBackTheSameSearchKeepsTheSelection() {
        var model = openModel()
        model.query = "s"
        model.moveSelection(by: 1)
        let selected = model.selected

        model.query = "s"

        XCTAssertEqual(model.selected, selected)
    }

    func testTheSelectionStopsAtBothEnds() {
        var model = openModel()
        model.moveSelection(by: -1)
        XCTAssertEqual(model.selected, model.items.first)

        model.moveSelection(by: 100)
        XCTAssertEqual(model.selected, model.items.last)
    }

    func testEscapeLeavesAPageThenClearsTheQueryThenCloses() {
        var model = openModel(isListening: true)
        model.query = "mac"
        model.show(.liveTranscript)

        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands)
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.query, "")
        XCTAssertTrue(model.escape())
    }

    func testCommandCommaOpensSettingsWhateverIsTypedButOnlyOnTheCommandsPage() {
        var model = openModel()
        model.query = "no match"
        let commandComma = Self.key(",", [.command, .capsLock])
        XCTAssertEqual(model.command(pressing: commandComma)?.action, .openSettings)
        XCTAssertNil(model.command(pressing: Self.key(",", [])))

        model.show(.liveTranscript)
        XCTAssertNil(model.command(pressing: commandComma))
    }

    func testAKeyRunsNoCommandThePageDoesNotList() {
        var model = openModel(history: [Self.entry(1, at: 100)])
        model.attach(Self.entry(1, at: 100))

        XCTAssertNil(model.command(pressing: Self.key(",", [.command])))
    }

    func testWithNothingTypedSavedDictationsReplaceTheCommandsBelowTheLiveRow() {
        var model = openModel(isListening: true)
        model.replace(entries: [Self.entry(2, at: 200), Self.entry(1, at: 100)])
        XCTAssertEqual(model.items.compactMap(\.command), model.commands)

        model.keepsHistory = true

        XCTAssertEqual(model.items.first?.command?.action, .showLiveTranscript)
        XCTAssertEqual(model.items.dropFirst().compactMap(\.entry?.id), [2, 1])
        XCTAssertEqual(model.items.count, 3)
        XCTAssertEqual(model.command(forNumber: 1)?.action, .runPrompt(id: prompts[0].id))
    }

    func testTypingShowsMatchingCommandsBeforeMatchingDictations() {
        var model = openModel(history: [Self.entry(1, at: 100)])

        model.query = "sett"

        XCTAssertEqual(model.items.map(\.id), ["settings", "entry-1"])
    }

    func testAnEmptyHistoryStillShowsTheCommands() {
        var model = openModel(history: [])

        XCTAssertEqual(model.items.compactMap(\.command), model.commands)
    }

    func testTheSelectedRowNamesItselfForVoiceOver() {
        var model = openModel(history: [Self.entry(1, at: 100)])
        model.query = "sett"
        XCTAssertEqual(model.selected?.spokenTitle, "Settings…")

        model.moveSelection(by: 1)

        XCTAssertEqual(model.selected?.spokenTitle, "Text 1")
    }

    func testAThreadedEntryReadsItsResultCountInTheRightNumber() {
        XCTAssertEqual(PaletteItem.entry(Self.threaded(1, at: 100, results: 1)).spokenTitle, "Text 1, 1 result")
        XCTAssertEqual(PaletteItem.entry(Self.threaded(2, at: 100, results: 3)).spokenTitle, "Text 2, 3 results")
    }

    func testATopSelectionMovesToTheNewestEntryAHistoryLoadBrings() {
        var model = openModel(history: [Self.entry(1, at: 100)])
        XCTAssertEqual(model.selected?.entry?.id, 1)

        model.replace(entries: [Self.entry(2, at: 200), Self.entry(1, at: 100)])

        XCTAssertEqual(model.selected?.entry?.id, 2)
    }

    func testARefreshKeepsTheSelectedDictationSelected() {
        var model = openModel(history: [Self.entry(2, at: 200), Self.entry(1, at: 100)])
        model.select("entry-1")

        model.replace(entries: [Self.entry(3, at: 300), Self.entry(2, at: 200), Self.entry(1, at: 100)])

        XCTAssertEqual(model.selected?.entry?.id, 1)
    }

    func testEachDayOfDictationsStartsWithItsTitle() {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC")!
        let day: TimeInterval = 86_400
        var model = openModel(history: [Self.entry(3, at: day + 10), Self.entry(2, at: day + 5), Self.entry(1, at: 10)])

        let titled = model.items.indices.map { model.dayStarting(at: $0, calendar: calendar) }

        XCTAssertEqual(titled, [Date(timeIntervalSince1970: day), nil, Date(timeIntervalSince1970: 0)])
    }

    func testNewPromptFollowsThePromptsOnlyUnderTheLimit() {
        var model = openModel()
        XCTAssertFalse(model.commands.contains(.newPrompt))

        model.canAddPrompt = true

        let actions = model.commands.map(\.action)
        XCTAssertEqual(
            actions.firstIndex(of: .newPrompt), actions.firstIndex(of: .runPrompt(id: prompts[9].id)).map { $0 + 1 })
    }

    func testAPromptSavedWhileOpenShowsAndTheSelectionStaysOnItsRow() {
        var model = openModel()
        model.moveSelection(by: 4)
        let selected = model.selected?.id
        let added = Aria_Daemon_V1_Prompt.stub(file: "/new.md")

        model.replace(prompts: [added] + prompts)

        XCTAssertEqual(model.selected?.id, selected)
        XCTAssertTrue(model.commands.contains { $0.action == .runPrompt(id: added.id) })
    }

    func testAFailedEditPromptSaysWhyUntilTheSelectionMovesOrThePaletteOpensAgain() throws {
        var model = openModel()
        model.moveSelection(by: 4)
        let row = try XCTUnwrap(model.selected?.id)

        model.failToEdit(on: row, .offline)
        XCTAssertEqual(model.promptFailure, .offline)

        model.moveSelection(by: 1)
        model.moveSelection(by: -1)
        XCTAssertNil(model.promptFailure)

        model.failToEdit(on: row, .offline)
        model.reset(prompts: prompts, isListening: false)
        XCTAssertNil(model.promptFailure)
    }

    func testEscapeLeavesAnUnchangedDraftButAsksBeforeDroppingAChangedOne() {
        var model = openModel()
        model.open(PromptDraft())
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands)
        XCTAssertNil(model.draft)

        model.open(PromptDraft())
        model.draft?.name = "Summary"
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .prompt)
        XCTAssertTrue(model.isConfirmingDiscard)

        model.discardDraft()
        XCTAssertEqual(model.page, .commands)
        XCTAssertNil(model.draft)
    }

    func testADraftKeptWhenThePaletteClosedComesBackForTheSamePromptOnly() {
        var model = openModel()
        model.open(PromptDraft())
        model.draft?.name = "Summary"

        model.reset(prompts: prompts, isListening: false)
        XCTAssertEqual(model.page, .commands)
        model.open(PromptDraft())
        XCTAssertEqual(model.draft?.name, "Summary")

        model.discardDraft()
        model.open(PromptDraft())
        model.open(PromptDraft(editing: prompts[0], contents: .text("Fix it.")))
        XCTAssertEqual(model.draft?.id, prompts[0].id, "an unchanged draft gives way")
    }

    func testASaveEndsOnlyTheDraftItSaved() {
        var model = openModel()
        model.open(PromptDraft(instructions: "List them.", kind: .compose))
        model.draft?.name = "First"
        let saving = model.draft!
        model.discardDraft()
        model.open(PromptDraft(instructions: "Summarize.", kind: .compose))
        model.draft?.name = "Second"
        var response = Aria_Daemon_V1_SavePromptResponse()
        response.id = "first"
        response.file = "/p/First.md"

        model.finishSave(of: saving, with: .failure(.full))
        model.finishSave(of: saving, with: .success(response))

        XCTAssertEqual(model.draft?.name, "Second")
        XCTAssertNil(model.draft?.failure)
        XCTAssertTrue(model.draft?.isNew == true)

        let second = model.draft!
        model.finishSave(of: second, with: .success(response))
        XCTAssertNil(model.draft, "an unchanged draft closes once saved")
    }

    func testOpeningAnotherDraftOverAChangedOneAsksFirst() {
        var model = openModel()
        model.open(PromptDraft(instructions: "List them.", kind: .compose))
        model.draft?.name = "Kept"

        model.open(PromptDraft(instructions: "Summarize.", kind: .compose), replacing: true)
        XCTAssertTrue(model.isConfirmingDiscard)
        model.keepDraft()
        XCTAssertEqual(model.draft?.name, "Kept")

        model.open(PromptDraft(editing: prompts[0], contents: .text("Fix it.")))
        XCTAssertTrue(model.isConfirmingDiscard)
        model.discardDraft()
        XCTAssertEqual(model.draft?.id, prompts[0].id)
        XCTAssertEqual(model.page, .prompt)
    }

    func testReturnTypesSavedTextButNotACommandOrAnEntryWithNothingToCopy() {
        var pending = Self.entry(1, at: 100)
        pending.status = .deletionPending
        let result = PromptResult(id: 1, created: .now, promptName: "Summarize", text: "Short")

        XCTAssertTrue(PaletteItem.entry(Self.entry(2, at: 100)).isTypable)
        XCTAssertTrue(PaletteItem.result(result).isTypable)
        XCTAssertFalse(PaletteItem.entry(pending).isTypable)
        XCTAssertFalse(PaletteItem.command(.settings).isTypable)
    }

    func testTypingWithAPromptInputAddsAOneOffRowLastAndAloneItIsSelected() {
        var model = openModel()
        model.attach(Self.entry(1, at: 100))
        XCTAssertFalse(model.commands.contains { $0.id == "one-off" })

        model.query = "  "
        XCTAssertFalse(model.commands.contains { $0.id == "one-off" })

        model.query = "list the action items"
        XCTAssertEqual(model.commands.map(\.action), [.runOneOff("list the action items")])
        XCTAssertEqual(model.selected?.command?.action, .runOneOff("list the action items"))

        model.query = prompts[0].name
        XCTAssertEqual(model.commands.last?.action, .runOneOff(prompts[0].name))
        XCTAssertEqual(model.selected?.command?.action, .runPrompt(id: prompts[0].id))
    }

    private static func replied(_ source: PromptSource) -> PaletteResult {
        var result = PaletteResult(promptName: "Prompt", source: source, input: entry(1, at: 100))
        result.started(ticket: 7)
        result.apply(
            .with {
                $0.ticket = 7
                $0.stage = .replied
                $0.text = "Done."
            })
        return result
    }

    private func resultActions(_ result: PaletteResult) -> [PaletteAction.Kind] {
        var model = openModel()
        model.show(.result)
        return PaletteActions(model: model, result: result).all.map(\.kind)
    }

    func testOnlyAOneOffThatRepliedCanBeSavedAsAPrompt() {
        XCTAssertEqual(
            resultActions(Self.replied(.oneOff(instructions: "Shorter."))),
            [.copyText("Done."), .saveAsPrompt(instructions: "Shorter.")])
        XCTAssertEqual(resultActions(Self.replied(.stored(id: "p"))), [.copyText("Done.")])
        XCTAssertEqual(
            resultActions(
                PaletteResult(
                    promptName: "Prompt", source: .oneOff(instructions: "Shorter."), input: Self.entry(1, at: 100))),
            [], "nothing while it runs")
    }

    func testOnlyAFailureThatCouldEndOtherwiseOffersTryAgain() {
        var retryable = PaletteResult(promptName: "Prompt", source: .stored(id: "p"), input: Self.entry(1, at: 100))
        retryable.fail(PaletteResult.Failure("Aria couldn’t run this prompt. Try again."))
        var final = PaletteResult(promptName: "Prompt", source: .stored(id: "p"), input: Self.entry(1, at: 100))
        final.fail(PaletteResult.Failure(code: .promptMissing))

        XCTAssertEqual(resultActions(retryable), [.retry])
        XCTAssertEqual(resultActions(final), [])
    }

    func testStopIsDisabledWhileTheSessionStops() {
        let model = openModel(isListening: true)
        XCTAssertEqual(model.selected?.command?.action, .showLiveTranscript)

        let running = PaletteActions(model: model, isListening: true).all
        let stopping = PaletteActions(model: model, isListening: true, isStopping: true).all

        XCTAssertEqual(running.map(\.kind), [.stop])
        XCTAssertEqual(running.map(\.isEnabled), [true])
        XCTAssertEqual(stopping.map(\.isEnabled), [false])
    }

    func testAListedEntryShowsOnlyRunPromptInTheFooter() {
        let model = openModel(history: [Self.entry(1, at: 100)])

        let footer = PaletteActions(model: model, hasFrontApp: true).all.filter(\.isInFooter)

        XCTAssertEqual(footer.map(\.id), ["run-prompt"])
    }

    func testReturnIsNamedForTypingOnlyWhenItTypesTheRow() {
        var model = openModel(history: [Self.entry(1, at: 100)])

        XCTAssertEqual(
            PaletteActions(model: model, hasFrontApp: true).returnTitle(typingInto: "Notes"), "Type into Notes")
        XCTAssertEqual(PaletteActions(model: model, hasFrontApp: false).returnTitle(typingInto: nil), "Open")
        XCTAssertEqual(
            PaletteActions(model: model, history: HistoryWork(isClearing: true), hasFrontApp: true)
                .returnTitle(typingInto: "Notes"),
            "Open", "nothing is typed while history clears")
    }

    func testTranscribeAgainIsOfferedOnlyOnTheOpenEntrysPage() {
        var entry = Self.entry(1, at: 100)
        entry.canRegenerate = true
        var model = openModel(history: [entry])
        XCTAssertFalse(PaletteActions(model: model).all.contains { $0.kind == .transcribeAgain })

        model.open(entry: entry.id)

        XCTAssertTrue(PaletteActions(model: model, detail: entry).all.contains { $0.kind == .transcribeAgain })
    }
}
