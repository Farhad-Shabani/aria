import SwiftUI
import XCTest

@testable import Aria

final class PaletteTests: XCTestCase {
    private let prompts = (1...10).map { Aria_Daemon_V1_Prompt.stub(file: "/\($0).md") }

    private static func entry(_ id: UInt64, at seconds: TimeInterval) -> HistoryItem {
        HistoryItem(id: id, created: Date(timeIntervalSince1970: seconds), text: "Text \(id)", status: .success)
    }

    private func openModel(isSystemTranscriptionRunning: Bool = false, history: [HistoryItem]? = nil) -> PaletteModel {
        var model = PaletteModel()
        model.reset(prompts: prompts, isSystemTranscriptionRunning: isSystemTranscriptionRunning)
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
        XCTAssertEqual(rows.last?.action, .openSettings(nil))
    }

    func testNewDictationAndAskFollowTranscriptionAndClearHistoryShowsOnlyWithHistoryOn() {
        var model = openModel()
        XCTAssertEqual(model.commands.prefix(3).map(\.action), [.startTranscription, .startDictation, .startAsk])
        XCTAssertFalse(model.commands.contains { $0.action == .clearHistory })

        model.keepsHistory = true

        XCTAssertEqual(model.commands.last?.action, .clearHistory)
    }

    func testASectionShowsOnlyItsEntriesAndEscapeWidensItBeforeClosing() {
        var model = openModel(isSystemTranscriptionRunning: true, history: [Self.entry(1, at: 100)])
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
        XCTAssertEqual(sections, [.dictation, .systemTranscription, .chat])
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
        XCTAssertEqual(model.section, .chat, "back past All wraps round")

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

    func testOnAnEntrysPageReturnTypesTheFocusedTextUntilAFollowUpQuotesIt() {
        var model = modelWithThread()
        model.open(entry: 2)
        let detail = Self.threaded(2, at: 200, results: 2)

        let resting = PaletteActions(model: model, detail: detail, hasFrontApp: true)
        XCTAssertEqual(resting.followUpReturn, .type("Text 2"))
        XCTAssertEqual(resting.returnTitle(typingInto: "Notes"), "Type into Notes")

        model.followUp = "Make it shorter"
        XCTAssertEqual(
            PaletteActions(model: model, detail: detail, hasFrontApp: true).followUpReturn,
            .start(quote: "Text 2", entry: 2))
        model.moveDetailFocus(by: 1, on: 2)
        let onResult = PaletteActions(model: model, detail: detail, hasFrontApp: false)
        XCTAssertEqual(
            onResult.followUpReturn, .start(quote: "Short.", entry: 2), "the focused result, with no app in front")
        XCTAssertEqual(onResult.returnTitle(typingInto: nil), "Ask")
    }

    func testAChatsDraftWaitsWhileATextOpenedFromItTakesItsOwn() {
        var model = modelOnChatPage()
        model.followUp = "And the budget?"

        model.open(entry: 2)
        XCTAssertEqual(model.followUp, "", "the entry's field starts empty")
        model.followUp = "Shorter?"
        XCTAssertFalse(model.escape(), "Esc clears the entry's draft first")
        XCTAssertFalse(model.escape())

        XCTAssertEqual(model.page, .chat)
        XCTAssertEqual(model.followUp, "And the budget?")
    }

    func testAChatsReplyTakesNoFollowUpOnItsEntryButItsResultsDo() {
        var model = modelWithThread()
        model.open(entry: 2)
        model.followUp = "Shorter?"
        var ask = Self.threaded(2, at: 200, results: 2)
        ask.kind = .ask(reply: "Noon.")

        XCTAssertNil(PaletteActions(model: model, detail: ask, hasFrontApp: true).followUpQuote)
        model.moveDetailFocus(by: 1, on: 2)
        XCTAssertEqual(
            PaletteActions(model: model, detail: ask, hasFrontApp: true).followUpReturn,
            .start(quote: "Short.", entry: 2))
    }

    func testATooLongOrPartlyReadTextTakesNoFollowUp() {
        var model = openModel(history: [])
        model.open(entry: 2)
        model.followUp = "Shorter?"
        var long = Self.entry(2, at: 200)
        long.text = String(repeating: "a", count: ChatAsk.quoteLimit + 1)
        var partial = Self.entry(2, at: 200)
        partial.kind = .systemTranscription(transcript: "/t.txt", read: .truncated)

        let tooLong = PaletteActions(model: model, detail: long, hasFrontApp: true)
        let partlyRead = PaletteActions(model: model, detail: partial, hasFrontApp: true)

        XCTAssertEqual(tooLong.quoteFits, false)
        XCTAssertNil(tooLong.followUpReturn)
        model.followUp = ""
        XCTAssertNil(
            PaletteActions(model: model, detail: long, hasFrontApp: true).followUpReturn,
            "too long to type, so Return offers nothing")
        XCTAssertNil(partlyRead.quoteFits)
        XCTAssertNil(partlyRead.followUpReturn)
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
        model.reset(prompts: [], isSystemTranscriptionRunning: false)

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

    func testHoverSelectsOnlyFromAPointerMoveUntilTheNextKeyOrOpen() {
        var model = openModel()
        let third = model.items[2].id
        XCTAssertFalse(model.hoverSelects(third))

        model.followsPointer = true
        XCTAssertTrue(model.hoverSelects(third))
        model.select(third)
        XCTAssertFalse(model.hoverSelects(third))

        model.toggleMenu(count: 3)
        XCTAssertTrue(model.hoverSelectsMenu(at: 2))
        XCTAssertFalse(model.hoverSelectsMenu(at: 0))

        model.reset(prompts: prompts, isSystemTranscriptionRunning: false)
        XCTAssertFalse(model.hoverSelects(third))
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

        model.reset(prompts: prompts, isSystemTranscriptionRunning: false)

        XCTAssertFalse(model.isConfirmingClear)
    }

    func testARunningSessionIsPinnedFirstAndSelectedInPlaceOfNewTranscription() {
        var model = openModel()
        XCTAssertEqual(model.commands.first?.action, .startTranscription)

        model.moveSelection(by: 2)
        model.isSystemTranscriptionRunning = true

        XCTAssertEqual(model.selected?.command?.action, .showLiveTranscript)
        XCTAssertFalse(model.commands.contains { $0.action == .startTranscription })
    }

    func testADigitRunsItsPromptOnlyOnTheCommandsPageWithNothingTyped() {
        var model = openModel(isSystemTranscriptionRunning: true)
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

        XCTAssertEqual(model.commands.map(\.action), [.openSettings(nil)])
        XCTAssertEqual(model.selected?.command?.action, .openSettings(nil))
    }

    func testTypingSettingsStillSelectsSettingsWhileSetupStepsAreLeft() {
        var model = openModel()
        model.replace(setupSteps: 1)

        model.query = "sett"

        XCTAssertEqual(model.selected?.command?.action, .openSettings(nil))
    }

    func testSearchingAFeaturesNameFindsTheCommandThatStartsIt() {
        var model = openModel()

        model.query = "mac audio"
        XCTAssertEqual(model.commands.map(\.action), [.startTranscription])

        model.query = "question"
        XCTAssertEqual(model.commands.map(\.action), [.startAsk])

        model.query = "chat"
        XCTAssertEqual(model.commands.map(\.action), [.startAsk])
    }

    func testSearchingHistoryFindsEverySectionBeforeClearing() {
        var model = openModel()
        model.keepsHistory = true

        model.query = "history"

        XCTAssertEqual(
            model.commands.map(\.action),
            HistorySection.allCases.map { .showHistory($0) } + [.clearHistory])
    }

    func testTheAskCommandNamesTheChosenAssistant() {
        var model = openModel()
        let askTitle = { model.commands.first { $0.action == .startAsk }?.title }

        XCTAssertEqual(askTitle(), "Ask Your Assistant", "the daemon hasn't answered")
        model.assistantName = "Codex"
        XCTAssertEqual(askTitle(), "Ask Codex")
    }

    func testSearchingSetupFindsSetUpAndFinishSettingUpWhileStepsAreLeft() {
        var model = openModel()
        XCTAssertFalse(model.commands.contains(.finishSetup))

        model.replace(setupSteps: 2)
        model.query = "setup"
        XCTAssertEqual(model.commands.map(\.action), [.openSetup, .openSettings(.finishSetup)])

        model.replace(setupSteps: 0)
        XCTAssertEqual(model.commands.map(\.action), [.openSetup])
    }

    func testSetupStepsChangingKeepsTheSelectionOnItsRow() {
        var model = openModel()
        model.moveSelection(by: model.items.count)
        XCTAssertEqual(model.selected?.command, .settings)

        model.replace(setupSteps: 1)
        XCTAssertEqual(model.selected?.command, .settings)

        model.replace(setupSteps: 0)
        XCTAssertEqual(model.selected?.command, .settings)
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
        var model = openModel(isSystemTranscriptionRunning: true)
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
        XCTAssertEqual(model.command(pressing: commandComma)?.action, .openSettings(nil))
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
        var model = openModel(isSystemTranscriptionRunning: true)
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
        let model = openModel(history: [])

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
        let model = openModel(history: [Self.entry(3, at: day + 10), Self.entry(2, at: day + 5), Self.entry(1, at: 10)])

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
        model.reset(prompts: prompts, isSystemTranscriptionRunning: false)
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

        model.reset(prompts: prompts, isSystemTranscriptionRunning: false)
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

    func testAFollowUpOnAResultQuotesItsReplyFromTheEntryItRanOn() {
        var model = openModel()
        model.show(.result)
        model.followUp = "Shorter?"

        XCTAssertEqual(
            PaletteActions(model: model, result: Self.replied(.stored(id: "p"))).followUpReturn,
            .start(quote: "Done.", entry: 1))
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
        let model = openModel(isSystemTranscriptionRunning: true)
        XCTAssertEqual(model.selected?.command?.action, .showLiveTranscript)

        let running = PaletteActions(model: model, isSystemTranscriptionRunning: true).all
        let stopping = PaletteActions(model: model, isSystemTranscriptionRunning: true, isStopping: true).all

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
        let model = openModel(history: [Self.entry(1, at: 100)])

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

    private static let chatKey = ChatKey(assistantID: .claudeCode, name: "c1")

    private static func chatRow(_ id: UInt64, at seconds: TimeInterval) -> HistoryItem {
        var row = entry(id, at: seconds)
        row.kind = .ask(reply: "Noon.")
        row.project = "/code/aria"
        row.chat = chatKey
        row.resultCount = 2
        return row
    }

    func testAChatRowTypesItsNewestReplyAndGoesAsAWhole() throws {
        let entry = Self.chatRow(2, at: 200)
        var model = openModel(history: [entry])
        let row = PaletteItem.chat(try XCTUnwrap(SavedChat(newest: entry)))
        XCTAssertEqual(model.items, [row])

        let actions = PaletteActions(model: model, hasFrontApp: true)

        XCTAssertTrue(actions.typesOnReturn(row), "as an entry's row does")
        XCTAssertEqual(actions.returnTitle(typingInto: "Notes"), "Type into Notes")
        XCTAssertEqual(
            actions.all.map(\.kind),
            [
                .copyEntry(entry), .renameChat(Self.chatKey),
                .deleteChat(Self.chatKey), .open(row),
            ])
        XCTAssertEqual(row.spokenTitle, "c1, chat", "the chat's name, not its newest question")
        XCTAssertNil(model.threadEntry, "its results stay with each ask")
        XCTAssertNil(model.prompt(forNumber: 1))
        model.openChat()
        XCTAssertEqual(model.page, .chat)
    }

    private static func repliedChat(question: String, reply: String) -> Chat {
        var ask = entry(2, at: 200)
        ask.kind = .ask(reply: reply)
        ask.text = question
        var chat = Chat(id: 1, key: chatKey, folder: "/code/aria")
        chat.merge(ChatPage(asks: [ask], older: nil))
        return chat
    }

    private func modelOnChatPage() -> PaletteModel {
        var model = openModel(history: [])
        model.openChat()
        return model
    }

    func testOnTheChatPageReturnTypesTheReplyUntilAFollowUpIsTyped() {
        var model = modelOnChatPage()
        let chat = Self.repliedChat(question: "Text 2", reply: "Noon.")

        let actions = PaletteActions(model: model, chat: chat, hasFrontApp: true)

        XCTAssertEqual(actions.followUpReturn, .type("Noon."))
        XCTAssertEqual(actions.returnTitle(typingInto: "Notes"), "Type into Notes")
        XCTAssertEqual(actions.all.filter(\.isInFooter).map(\.kind), [.copyText("Noon.")], "Return names typing")
        XCTAssertNil(
            PaletteActions(model: model, chat: chat, hasFrontApp: false).followUpReturn,
            "no app to type into")
        XCTAssertEqual(actions.all.first { $0.kind == .typeText("Noon.") }?.key, .typeText)
        XCTAssertEqual(actions.all.first { $0.kind == .copyText("Noon.") }?.key, .copy)
        XCTAssertEqual(actions.all.map(\.id), ["copy", "type", "run-prompt", "rename", "delete-chat"])

        model.followUp = "And dinner?"
        let typed = PaletteActions(model: model, chat: chat, hasFrontApp: true)
        XCTAssertEqual(typed.followUpReturn, .send)
        XCTAssertEqual(typed.returnTitle(typingInto: "Notes"), "Send")

        model.followUp = ""
        model.keepsHistory = false
        let unsaved = PaletteActions(model: model, chat: chat, hasFrontApp: true)
        XCTAssertEqual(unsaved.all.map(\.id), ["copy", "type"], "nothing saved to run on or delete")
    }

    func testAChatShowsItsNameAndRenamesInPlace() throws {
        let entry = Self.chatRow(2, at: 200)
        var model = openModel(history: [entry])
        let chat = try XCTUnwrap(SavedChat(newest: entry))
        XCTAssertEqual(chat.name, "c1")
        XCTAssertEqual(PaletteItem.chat(chat).spokenTitle, "c1, chat")

        model.startRenaming(Self.chatKey)

        XCTAssertTrue(model.isRenaming)
        XCTAssertEqual(model.renaming, .chat(Self.chatKey))
        XCTAssertNil(model.renamingID, "no entry is being renamed")
        XCTAssertEqual(model.renamingTitle, "c1")
        XCTAssertTrue(PaletteActions(model: model, hasFrontApp: true).all.isEmpty, "the field has the keys")
        XCTAssertFalse(model.escape(), "Esc ends the rename")
        XCTAssertFalse(model.isRenaming)
    }

    func testTheKeysStepThroughQuestionsAndRepliesAndStopAtTheEnds() {
        var chat = Self.repliedChat(question: "Text 2", reply: "Noon.")
        chat.append(ChatAsk(live: 1, question: "And dinner?"))
        let asks = chat.asks
        var selection = MessageSelection()
        XCTAssertTrue(selection.isResting)
        XCTAssertEqual(selection.selected(in: asks).map(\.side), [.reply], "the newest reply")

        selection.move(by: -1, extending: false, in: asks)
        XCTAssertEqual(selection.selected(in: asks), [ChatMessage(ask: asks[1].id, side: .question)])
        selection.move(by: -1, extending: false, in: asks)
        XCTAssertEqual(selection.text(in: asks), "Noon.")
        selection.move(by: -5, extending: false, in: asks)
        XCTAssertEqual(selection.text(in: asks), "Text 2", "the first question, and no further")
        XCTAssertFalse(selection.isResting)
    }

    func testShiftExtendsASelectionAndItsTextJoinsInOrderWithoutAReplyStillRunning() {
        var chat = Self.repliedChat(question: "Text 2", reply: "Noon.")
        chat.append(ChatAsk(live: 1, question: "And dinner?"))
        let asks = chat.asks
        var selection = MessageSelection()
        selection.move(by: -3, extending: false, in: asks)

        selection.move(by: 1, extending: true, in: asks)
        selection.move(by: 1, extending: true, in: asks)
        XCTAssertEqual(selection.text(in: asks), "Text 2\n\nNoon.\n\nAnd dinner?")
        selection.move(by: 1, extending: true, in: asks)
        XCTAssertEqual(selection.selected(in: asks).count, 4)
        XCTAssertEqual(selection.text(in: asks), "Text 2\n\nNoon.\n\nAnd dinner?", "the running reply has no text yet")

        XCTAssertTrue(selection.collapse(in: asks))
        XCTAssertEqual(selection.selected(in: asks).count, 1)
        XCTAssertFalse(selection.collapse(in: asks), "one already")

        selection.selectAll(in: asks)
        XCTAssertEqual(selection.selected(in: asks).count, 4)
        selection.move(by: -1, extending: false, in: asks)
        XCTAssertEqual(selection.selected(in: asks).count, 1, "a plain move drops the rest")
    }

    func testCopyAndReturnTakeTheSelectedQuestionAlone() {
        var model = modelOnChatPage()
        let chat = Self.repliedChat(question: "Text 2", reply: "Noon.")
        model.messageSelection.move(by: -1, extending: false, in: chat.asks)

        let actions = PaletteActions(model: model, chat: chat, hasFrontApp: true)

        XCTAssertEqual(actions.all.first?.kind, .copyText("Text 2"))
        XCTAssertEqual(actions.followUpReturn, .type("Text 2"))
    }

    func testARowWhoseChatRepliesOffItsPageCanStopIt() throws {
        var model = openModel(history: [Self.entry(1, at: 100), Self.chatRow(2, at: 200)])
        var quoting = Chat(id: 1, key: Self.chatKey, folder: "/code/aria", quote: "Text 1")
        quoting.append(ChatAsk(live: 1, question: "Shorter?", from: 1))
        let stop = PaletteAction(kind: .stopAsk(Self.chatKey), isInFooter: true)

        model.select("entry-1")
        XCTAssertTrue(PaletteActions(model: model, replying: quoting).all.contains(stop), "the entry it quotes")
        XCTAssertFalse(PaletteActions(model: model).all.contains(stop), "nothing replying")
        let chatRow = try XCTUnwrap(model.items.first { if case .chat = $0 { true } else { false } })
        model.select(chatRow.id)
        let onChatRow = PaletteActions(model: model, replying: quoting).all
        XCTAssertTrue(onChatRow.contains(stop), "its own row")
        XCTAssertEqual(onChatRow.first { $0.id == "delete-chat" }?.isEnabled, false, "it refuses a delete")
    }

    func testStopShowsOnlyWhileAChatTheDaemonNamedReplies() {
        let model = modelOnChatPage()
        var unnamed = Chat(id: 1, key: nil, folder: "/code/aria")
        unnamed.append(ChatAsk(live: 1, question: "When is lunch?"))
        var named = Self.repliedChat(question: "Text 2", reply: "Noon.")
        named.append(ChatAsk(live: 2, question: "And dinner?"))

        let stops = { (chat: Chat) in
            PaletteActions(model: model, chat: chat).all.contains { $0.kind == .stopAsk(Self.chatKey) }
        }

        XCTAssertFalse(stops(unnamed), "nothing to stop it by yet")
        XCTAssertTrue(stops(named))
        XCTAssertFalse(stops(Self.repliedChat(question: "Text 2", reply: "Noon.")))
    }

    func testAskFollowsUpOnlyInTheChatOnScreen() {
        var model = openModel(history: [])
        let chat = Self.repliedChat(question: "Text 2", reply: "Noon.")
        XCTAssertEqual(model.askTarget(shown: chat), .new, "the commands page")

        model.openChat()
        XCTAssertEqual(model.askTarget(shown: chat), .followUp(Self.chatKey))

        model.open(entry: 2)
        XCTAssertEqual(model.askTarget(shown: chat), .new, "an ask's entry page")
        XCTAssertFalse(model.escape())
        model.attach(Self.chatRow(2, at: 200))
        XCTAssertEqual(model.askTarget(shown: chat), .new, "the prompts to run on an ask")
        model.show(.result)
        XCTAssertEqual(model.askTarget(shown: chat), .new, "a prompt's result")

        model.openChat()
        XCTAssertNil(model.input, "the chat shows with no prompt input left over")
    }

    func testEscapeFromAPageOpenedOnTheChatGoesBackToIt() {
        var model = openModel(history: [Self.entry(1, at: 100)])
        model.openChat()
        model.open(entry: 2)

        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .chat)

        model.attach(Self.chatRow(2, at: 200))
        model.show(.result)
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands, "the result goes back to the prompts")
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .chat)
        XCTAssertNil(model.input)

        model.followUp = "And dinner?"
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.followUp, "", "a typed follow-up clears first")
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands)
        XCTAssertFalse(model.returnsToChat)
        model.open(entry: 1)
        XCTAssertFalse(model.escape())
        XCTAssertEqual(model.page, .commands, "an entry opened from the list goes back to it")
    }
}
