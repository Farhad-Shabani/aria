import XCTest

@testable import Aria

@MainActor
final class HistoryStateTests: XCTestCase {
    @MainActor
    private final class Harness {
        var entries = [item(2), item(1)]
        var deletionFails = false
        var copied = [String]()
        var listedSections = [HistorySection?]()
        var copySucceeds = true
        var regenerateContinuation: CheckedContinuation<HistoryItem, Error>?
        var clearCalls = 0
        var renamedChats: [ChatKey] = []
        var chatRenameFailure: HistoryFailure?
        var clearFailure: HistoryFailure?
        var isCleanupPending = false

        var effects: HistoryState.Effects {
            .init(
                list: { _, section, _ in
                    self.listedSections.append(section)
                    return HistoryPage(items: self.entries.filter { section == nil || $0.kind.section == section })
                },
                get: { id in
                    guard let entry = self.entries.first(where: { $0.id == id }) else {
                        throw HistoryFailure.missing
                    }
                    return entry
                },
                thread: { _ in [] },
                rename: { id, title in
                    guard let index = self.entries.firstIndex(where: { $0.id == id }) else {
                        throw HistoryFailure.missing
                    }
                    self.entries[index].title = title
                    return self.entries[index]
                },
                renameChat: { chat, name in
                    self.renamedChats.append(chat)
                    if let failure = self.chatRenameFailure { throw failure }
                    let renamed = ChatKey(assistantID: chat.assistantID, name: name)
                    for index in self.entries.indices where self.entries[index].chat == chat {
                        self.entries[index].chat = renamed
                    }
                    return renamed
                },
                delete: { id in
                    if self.deletionFails { throw HistoryFailure.storage }
                    self.entries.removeAll { $0.id == id }
                },
                deleteChat: { chat in
                    self.entries.removeAll { $0.chat == chat }
                },
                clear: {
                    self.clearCalls += 1
                    if let error = self.clearFailure { throw error }
                    let deletedEntries = UInt64(self.entries.count)
                    self.entries = []
                    return HistoryClearResult(deletedEntries: deletedEntries, isCleanupPending: self.isCleanupPending)
                },
                regenerate: { _ in try await withCheckedThrowingContinuation { self.regenerateContinuation = $0 } },
                copy: { text in
                    self.copied.append(text)
                    return self.copySucceeds
                }
            )
        }
    }

    private static let chat = ChatKey(assistantID: .claudeCode, name: "c1")

    private static func item(_ id: UInt64) -> HistoryItem {
        HistoryItem(id: id, created: Date(timeIntervalSince1970: 100), text: "Text \(id)", status: .success)
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    func testRenamingAChatMovesEachOfItsAsksAndAnEmptyOrTakenNameDoesNot() async {
        let harness = Harness()
        harness.entries = ([1, 2, 3] as [UInt64]).map { id in
            var entry = Self.item(id)
            entry.kind = .ask(reply: "Noon.")
            entry.chat = id == 3 ? nil : Self.chat
            return entry
        }
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.items.count == 3 }
        var heard: [ChatKey] = []
        let renamed = ChatKey(assistantID: Self.chat.assistantID, name: "Lunch plans")

        state.renameChat(Self.chat, to: "  Lunch plans ") { heard.append($0) }

        await waitUntil { heard == [renamed] }
        XCTAssertEqual(state.items.map(\.chat), [renamed, renamed, nil])
        for unchanged in [" ", "Lunch plans"] {
            state.renameChat(renamed, to: unchanged) { heard.append($0) }
        }
        state.renameChat(renamed, to: String(repeating: "x", count: ChatKey.nameLimit + 1))
        XCTAssertEqual(state.error, .nameTooLong)
        harness.chatRenameFailure = .chatNameTaken
        state.renameChat(renamed, to: "Dinner")
        await waitUntil { state.error == .chatNameTaken }
        harness.chatRenameFailure = .busy
        state.renameChat(renamed, to: "Dinner")
        await waitUntil { state.error == .chatRenameReplying }

        XCTAssertEqual(harness.renamedChats, [Self.chat, renamed, renamed])
        XCTAssertEqual(heard, [renamed], "a failed rename names nothing")
        XCTAssertEqual(state.items.map(\.chat), [renamed, renamed, nil])
    }

    func testRefreshPreservesSelectionAndFetchesFullText() async {
        let harness = Harness()
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail?.id == 2 }
        state.select(1)
        await waitUntil { state.detail?.id == 1 }
        harness.entries[1].text = "Full updated text"
        state.refresh()
        await waitUntil { state.detail?.text == "Full updated text" }
        XCTAssertEqual(state.selectedID, 1)
        state.close()
    }

    func testADictationEndingRefreshesOnlyAnOpenWindow() async {
        let harness = Harness()
        var lists = 0
        var effects = harness.effects
        effects.list = { _, _, _ in
            lists += 1
            return HistoryPage(items: harness.entries)
        }
        let state = HistoryState(effects: effects)

        state.dictationEnded()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(lists, 0, "a closed window has nothing to refresh")

        state.open()
        await waitUntil { state.detail != nil }
        XCTAssertEqual(lists, 1)
        state.dictationEnded()
        await waitUntil { lists == 2 }

        state.close()
        state.dictationEnded()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(lists, 2)
    }

    func testOpeningListsEverythingRatherThanTheLastSearch() async {
        let harness = Harness()
        var queries = [String]()
        var effects = harness.effects
        effects.list = { query, _, _ in
            queries.append(query)
            return HistoryPage(items: harness.entries)
        }
        let state = HistoryState(effects: effects)
        state.filter(query: "old", section: nil)
        state.close()

        state.open()

        await waitUntil { !queries.isEmpty }
        XCTAssertEqual(queries, [""])
        XCTAssertEqual(state.query, "")
        state.close()
    }

    func testDaysGroupNewestFirstAndKeepOrderWithinADay() {
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = .gmt
        let day: TimeInterval = 86_400
        func entry(_ id: UInt64, at seconds: TimeInterval) -> HistoryItem {
            HistoryItem(id: id, created: Date(timeIntervalSince1970: seconds), text: "", status: .success)
        }
        let items = [entry(3, at: day + 50), entry(2, at: 20), entry(1, at: 10)]

        let days = HistoryDay.grouping(items, calendar: utc)

        XCTAssertEqual(days.map(\.start), [Date(timeIntervalSince1970: day), Date(timeIntervalSince1970: 0)])
        XCTAssertEqual(days.map { $0.items.map(\.id) }, [[3], [2, 1]])
    }

    func testDaysFollowTheListedItems() async {
        let harness = Harness()
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { !state.items.isEmpty }
        XCTAssertEqual(state.days, HistoryDay.grouping(state.items))
        state.close()
    }

    func testSupersededSearchCannotReplaceNewResults() async {
        let harness = Harness()
        var continuations: [CheckedContinuation<HistoryPage, Error>] = []
        var effects = harness.effects
        effects.list = { _, _, _ in try await withCheckedThrowingContinuation { continuations.append($0) } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { continuations.count == 1 }
        state.filter(query: "new", section: nil)
        state.refresh()
        await waitUntil { continuations.count == 2 }
        continuations[1].resume(returning: HistoryPage(items: [Self.item(1)]))
        await waitUntil { state.detail?.id == 1 }
        continuations[0].resume(returning: HistoryPage(items: [Self.item(2)]))
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(state.items.map(\.id), [1])
        XCTAssertEqual(state.query, "new")
        state.close()
    }

    func testRefreshKeepsSelectionBeyondFirstPage() async {
        let harness = Harness()
        var effects = harness.effects
        effects.list = { _, _, before in
            before == nil
                ? HistoryPage(items: [harness.entries[0]], next: 2)
                : HistoryPage(items: [harness.entries[1]])
        }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { state.detail?.id == 2 }
        state.loadMore(after: 2)
        await waitUntil { state.items.count == 2 }
        state.select(1)
        await waitUntil { state.detail?.id == 1 }
        state.refresh()
        await waitUntil { !state.isLoading && state.detail?.id == 1 }
        XCTAssertEqual(state.items.map(\.id), [2, 1])
        XCTAssertEqual(state.selectedID, 1)
        state.close()
    }

    func testALaterPageDropsAChatAlreadyShown() async {
        let harness = Harness()
        var newest = Self.item(3)
        newest.chat = Self.chat
        var older = Self.item(1)
        older.chat = Self.chat
        var effects = harness.effects
        effects.list = { _, _, before in
            before == nil
                ? HistoryPage(items: [newest], next: 3)
                : HistoryPage(items: [older, harness.entries[0]])
        }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { state.items.count == 1 && !state.isLoading }

        state.loadMore(after: 3)

        await waitUntil { !state.isLoading }
        XCTAssertEqual(state.items.map(\.id), [3, 2])
        state.close()
    }

    func testSupersededSelectionCannotReplaceCurrentDetail() async {
        let harness = Harness()
        var reads: [UInt64: CheckedContinuation<HistoryItem, Error>] = [:]
        var effects = harness.effects
        effects.get = { id in try await withCheckedThrowingContinuation { reads[id] = $0 } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { reads[2] != nil }
        state.select(1)
        await waitUntil { reads[1] != nil }
        reads[1]?.resume(returning: Self.item(1))
        await waitUntil { state.detail?.id == 1 }
        reads[2]?.resume(returning: Self.item(2))
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(state.detail?.id, 1)
        state.close()
    }

    func testFailedDeleteKeepsEntryAndReportsStorageFailure() async {
        let harness = Harness()
        harness.deletionFails = true
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.delete(2)
        await waitUntil { state.error == .storage }
        XCTAssertEqual(state.items.map(\.id), [2, 1])
        XCTAssertEqual(state.selectedID, 2)
        state.close()
    }

    func testDeleteSelectsNeighborAfterAcknowledgement() async {
        let harness = Harness()
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.delete(2)
        await waitUntil { state.detail?.id == 1 }
        XCTAssertEqual(state.items.map(\.id), [1])
        state.close()
    }

    func testAnEntryTheDaemonIsTranscribingCannotBeTranscribedAgain() async {
        let harness = Harness()
        harness.entries[0].canRegenerate = true
        harness.entries[0].isRegenerating = true
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }

        state.regenerate()

        XCTAssertNil(state.regeneratingID)
        XCTAssertNil(harness.regenerateContinuation)
        state.close()
    }

    func testRetryAfterSelectionChangeUpdatesOwnRowWithoutCopy() async {
        let harness = Harness()
        harness.entries[0].canRegenerate = true
        harness.entries[0].status = .transcriptionFailed
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.regenerate()
        await waitUntil { harness.regenerateContinuation != nil }
        state.clear()
        XCTAssertFalse(state.isClearing)
        XCTAssertEqual(harness.clearCalls, 0)
        state.delete(2)
        XCTAssertNil(state.deletingID)
        state.select(1)
        await waitUntil { state.detail?.id == 1 }
        harness.entries[0].text = "Recovered text"
        harness.entries[0].status = .recovered
        harness.entries[0].canRegenerate = false
        harness.regenerateContinuation?.resume(returning: harness.entries[0])
        await waitUntil { state.regeneratingID == nil && !state.isLoading && state.detail?.id == 1 }
        XCTAssertEqual(state.items.first?.text, "Recovered text")
        XCTAssertEqual(state.detail?.text, "Text 1")
        XCTAssertTrue(harness.copied.isEmpty)
        state.close()
    }

    func testCloseCancelsRetryWithoutReplacingEntry() async {
        let harness = Harness()
        harness.entries[0].canRegenerate = true
        var effects = harness.effects
        var cancelled = false
        effects.regenerate = { _ in
            do { try await Task.sleep(for: .seconds(60)) } catch {
                cancelled = Task.isCancelled
                throw error
            }
            return Self.item(2)
        }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.regenerate()
        await Task.yield()
        state.close()
        await waitUntil { cancelled }
        XCTAssertEqual(state.detail?.text, "Text 2")
    }

    func testReopenDuringCancelledRetryRefreshesWhenServerReleasesEntry() async {
        let harness = Harness()
        harness.entries[0].status = .transcriptionFailed
        harness.entries[0].canRegenerate = true
        var effects = harness.effects
        var poll: CheckedContinuation<Void, Error>?
        effects.waitForRegenerate = { try await withCheckedThrowingContinuation { poll = $0 } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.regenerate()
        await waitUntil { harness.regenerateContinuation != nil }
        harness.entries[0].isRegenerating = true
        harness.entries[0].canRegenerate = false
        state.close()
        state.refresh()
        await waitUntil { state.detail?.isRegenerating == true && poll != nil }

        harness.entries[0].isRegenerating = false
        harness.entries[0].canRegenerate = true
        harness.regenerateContinuation?.resume(throwing: HistoryFailure.cancelled)
        poll?.resume()
        await waitUntil { state.detail?.canRegenerate == true && state.regeneratingID == nil }
        XCTAssertFalse(state.detail?.isRegenerating ?? true)
        XCTAssertFalse(state.items[0].isRegenerating)
        XCTAssertNil(state.error)
        XCTAssertTrue(harness.copied.isEmpty)
        state.close()
    }

    func testCloseStopsRetryStatusPolling() async {
        let harness = Harness()
        harness.entries[0].isRegenerating = true
        var effects = harness.effects
        var reads = 0
        var poll: CheckedContinuation<Void, Error>?
        effects.get = { _ in
            reads += 1
            return harness.entries[0]
        }
        effects.waitForRegenerate = { try await withCheckedThrowingContinuation { poll = $0 } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { poll != nil }
        state.close()
        poll?.resume()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(reads, 1)
    }

    func testNarrowingToASectionListsOnlyItAndOpeningWidensAgain() async {
        let harness = Harness()
        var ask = Self.item(3)
        ask.kind = .ask(reply: "reply")
        harness.entries = [ask, Self.item(2)]
        let state = HistoryState(effects: harness.effects)
        state.open()
        await waitUntil { state.items.count == 2 }

        state.filter(query: "", section: .chat)
        await waitUntil { state.items.count == 1 }

        XCTAssertEqual(state.items.map(\.id), [3])
        state.close()
        state.open()
        await waitUntil { state.items.count == 2 }
        XCTAssertEqual(harness.listedSections.last, .some(nil))
        state.close()
    }

    func testCopyingAChatTakesItsReply() async {
        let harness = Harness()
        var ask = Self.item(3)
        ask.kind = .ask(reply: "The reply")
        harness.entries = [ask]
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }

        state.copy()

        XCTAssertEqual(harness.copied, ["The reply"])
        state.close()
    }

    func testTypingARowTakesTheWholeText() async {
        let harness = Harness()
        harness.entries[0].text = "The whole text"
        let state = HistoryState(effects: harness.effects)

        let whole = await state.typingText(of: 2)

        XCTAssertEqual(whole, "The whole text")
    }

    func testCopyingARowTakesTheWholeTextRatherThanWhatTheListHolds() async {
        let harness = Harness()
        harness.entries[0].text = "The whole text"
        let state = HistoryState(effects: harness.effects)

        state.copy(2)
        await waitUntil { !harness.copied.isEmpty }

        XCTAssertEqual(harness.copied, ["The whole text"])
        XCTAssertEqual(state.notice, "Copied")
        state.close()
    }

    func testDeletingAChatRemovesItsRowAndLeavesTheRest() async {
        let harness = Harness()
        harness.entries[0].chat = Self.chat
        let state = HistoryState(effects: harness.effects)
        state.open()
        await waitUntil { state.items.count == 2 }

        state.deleteChat(Self.chat)

        await waitUntil { state.items.map(\.id) == [1] && state.deletingID == nil }
        XCTAssertEqual(harness.entries.map(\.id), [1])
        state.close()
    }

    func testAChatStillReplyingSaysWhyItWasNotDeleted() async {
        let harness = Harness()
        harness.entries[0].chat = Self.chat
        var effects = harness.effects
        effects.deleteChat = { _ in throw HistoryFailure.busy }
        let state = HistoryState(effects: effects)
        state.open()
        await waitUntil { state.items.count == 2 }

        state.deleteChat(Self.chat)

        await waitUntil { state.error != nil && state.deletingID == nil }
        XCTAssertEqual(state.error?.message, "Aria can’t delete this chat while it’s replying.")
        XCTAssertEqual(state.items.map(\.id), [2, 1])
        state.close()
    }

    func testAnEarlierRenameThatAnswersLastDoesNotWin() async {
        let harness = Harness()
        var renames: [CheckedContinuation<HistoryItem, Error>] = []
        var effects = harness.effects
        effects.rename = { _, _ in try await withCheckedThrowingContinuation { renames.append($0) } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { state.detail?.id == 2 }

        state.rename(2, to: "Old")
        await waitUntil { renames.count == 1 }
        state.rename(2, to: "New")
        await waitUntil { renames.count == 2 }
        var renamed = Self.item(2)
        renamed.title = "New"
        renames[1].resume(returning: renamed)
        await waitUntil { state.detail?.title == "New" }
        renamed.title = "Old"
        renames[0].resume(returning: renamed)
        for _ in 0..<20 { await Task.yield() }

        XCTAssertEqual(state.detail?.title, "New")
        XCTAssertEqual(state.items.first?.title, "New")
        state.close()
    }

    func testRenamingTrimsTheNameAndABlankNameRemovesTheTitle() async {
        let harness = Harness()
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail?.id == 2 }

        state.rename(2, to: "  Standup notes ")
        await waitUntil { state.detail?.title != nil }
        XCTAssertEqual(state.items.first?.name, "Standup notes")

        state.rename(2, to: "   ")
        await waitUntil { state.detail?.title == nil }
        XCTAssertEqual(state.items.first?.name, "Text 2")
        state.close()
    }

    func testANameOverTheLimitIsRefusedBeforeItIsSent() {
        let harness = Harness()
        let state = HistoryState(effects: harness.effects)

        state.rename(2, to: String(repeating: "é", count: 129))

        XCTAssertEqual(state.error, .nameTooLong)
        XCTAssertNil(harness.entries.first?.title)
    }

    func testFailedClipboardWriteDoesNotClaimCopied() async {
        let harness = Harness()
        harness.copySucceeds = false
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.copy()
        XCTAssertEqual(harness.copied, ["Text 2"])
        XCTAssertEqual(state.error, .copyFailed, "a failure shows as an error, not a notice")
        XCTAssertNil(state.notice)

        harness.copySucceeds = true
        state.copy()
        XCTAssertNil(state.error, "copying again replaces the failure")
        XCTAssertEqual(state.notice, "Copied")
        state.close()
    }

    func testClearResetsSearchPagesAndSelectionAndPreservesCleanupNotice() async {
        let harness = Harness()
        harness.isCleanupPending = true
        var effects = harness.effects
        var requests: [(String, UInt64?)] = []
        var warning: String?
        effects.list = { query, _, before in
            requests.append((query, before))
            guard !harness.entries.isEmpty else { return HistoryPage(items: [], warning: warning) }
            return before == nil
                ? HistoryPage(items: [harness.entries[0]], next: 2)
                : HistoryPage(items: [harness.entries[1]])
        }
        let state = HistoryState(effects: effects)
        state.filter(query: "Text", section: nil)
        state.refresh()
        await waitUntil { state.detail?.id == 2 }
        state.loadMore(after: 2)
        await waitUntil { state.items.count == 2 }
        state.select(1)
        await waitUntil { state.detail?.id == 1 }
        state.clear()
        await waitUntil { harness.clearCalls == 1 && !state.isClearing && !state.isLoading }
        XCTAssertTrue(state.items.isEmpty)
        XCTAssertNil(state.selectedID)
        XCTAssertNil(state.detail)
        XCTAssertEqual(requests.last?.0, "Text")
        XCTAssertNil(requests.last?.1)
        let notice = "History cleared. Aria will finish removing some recordings later."
        XCTAssertEqual(state.notice, notice)
        state.refresh()
        await waitUntil { !state.isLoading }
        XCTAssertNil(state.notice)
        warning = "History storage needs attention."
        state.refresh()
        await waitUntil { !state.isLoading }
        XCTAssertEqual(state.notice, warning)
        XCTAssertTrue(state.canClear)
        harness.isCleanupPending = false
        warning = nil
        state.clear()
        await waitUntil { harness.clearCalls == 2 && !state.isClearing && !state.isLoading }
        XCTAssertNil(state.notice)
        state.close()
    }

    func testClearInvalidatesReadsAndBlocksOverlappingOperations() async {
        let harness = Harness()
        var lists: [CheckedContinuation<HistoryPage, Error>] = []
        var read: CheckedContinuation<HistoryItem, Error>?
        var clear: CheckedContinuation<HistoryClearResult, Error>?
        var effects = harness.effects
        effects.list = { _, _, _ in try await withCheckedThrowingContinuation { lists.append($0) } }
        effects.get = { _ in try await withCheckedThrowingContinuation { read = $0 } }
        effects.clear = { try await withCheckedThrowingContinuation { clear = $0 } }
        let state = HistoryState(effects: effects)
        state.refresh()
        await waitUntil { lists.count == 1 }
        lists[0].resume(returning: HistoryPage(items: harness.entries, next: 1))
        await waitUntil { read != nil }
        state.refresh()
        await waitUntil { lists.count == 2 }
        state.clear()
        await waitUntil { clear != nil }
        state.clear()
        state.delete(2)
        state.regenerate()
        state.refresh()
        state.loadMore(after: 1)
        state.select(1)
        XCTAssertTrue(state.isClearing)
        XCTAssertFalse(state.canClear)
        XCTAssertNil(state.deletingID)
        XCTAssertNil(state.regeneratingID)
        XCTAssertEqual(state.selectedID, 2)
        XCTAssertEqual(lists.count, 2)
        clear?.resume(returning: HistoryClearResult(deletedEntries: 2, isCleanupPending: false))
        await waitUntil { lists.count == 3 }
        lists[2].resume(returning: HistoryPage(items: []))
        await waitUntil { !state.isLoading }
        lists[1].resume(returning: HistoryPage(items: harness.entries))
        read?.resume(returning: Self.item(2))
        for _ in 0..<20 { await Task.yield() }
        XCTAssertTrue(state.items.isEmpty)
        XCTAssertNil(state.detail)
        XCTAssertNil(state.selectedID)
        state.close()
    }

    func testFailedClearPreservesErrorWhileRefreshingAuthoritativeState() async {
        for failure in [HistoryFailure.busy, .storage, .clearUncertain, .unsupported] {
            let harness = Harness()
            harness.clearFailure = failure
            let state = HistoryState(effects: harness.effects)
            state.refresh()
            await waitUntil { state.detail != nil }
            harness.entries = [Self.item(1)]
            state.clear()
            await waitUntil {
                harness.clearCalls == 1 && !state.isClearing && !state.isLoading && state.detail?.id == 1
            }
            XCTAssertEqual(state.error, failure)
            XCTAssertEqual(state.items.map(\.id), [1])
            XCTAssertTrue(state.canClear)
            state.close()
        }
    }

    func testClearIsBlockedByKnownRetry() async {
        let harness = Harness()
        harness.entries[1].isRegenerating = true
        let state = HistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        XCTAssertFalse(state.canClear)
        state.clear()
        XCTAssertFalse(state.isClearing)
        XCTAssertEqual(harness.clearCalls, 0)
        state.close()
    }

    func testAFilterSetDuringAClearListsOnceTheClearFails() async {
        let harness = Harness()
        var clear: CheckedContinuation<HistoryClearResult, Error>?
        var effects = harness.effects
        effects.clear = { try await withCheckedThrowingContinuation { clear = $0 } }
        let state = HistoryState(effects: effects)
        state.open()
        await waitUntil { !state.isLoading }
        state.clear()
        await waitUntil { clear != nil }

        state.filter(query: "", section: .chat)
        clear?.resume(throwing: HistoryFailure.storage)

        await waitUntil { !state.isClearing && !state.isLoading }
        XCTAssertEqual(harness.listedSections.last, .chat)
        state.close()
    }

    func testCloseDuringClearReleasesControlsAndIgnoresLateCompletion() async {
        let harness = Harness()
        var clears: [CheckedContinuation<HistoryClearResult, Error>] = []
        var effects = harness.effects
        effects.clear = { try await withCheckedThrowingContinuation { clears.append($0) } }
        let state = HistoryState(effects: effects)
        state.clear()
        await waitUntil { clears.count == 1 }
        state.close()
        XCTAssertFalse(state.isClearing)
        XCTAssertEqual(state.error, .clearUncertain)
        state.refresh()
        await waitUntil { state.detail != nil }
        XCTAssertEqual(state.error, .clearUncertain)
        state.clear()
        await waitUntil { clears.count == 2 }
        clears[0].resume(returning: HistoryClearResult(deletedEntries: 2, isCleanupPending: false))
        for _ in 0..<20 { await Task.yield() }
        XCTAssertTrue(state.isClearing)
        XCTAssertEqual(state.items.count, 2)
        clears[1].resume(throwing: HistoryFailure.busy)
        await waitUntil { !state.isClearing && !state.isLoading }
        XCTAssertEqual(state.error, .busy)
        state.close()
    }

    func testClearFailuresDistinguishUnsupportedAndUncertainCompletion() {
        XCTAssertEqual(HistoryFailure(.unimplemented), .unsupported)
        for failure in [HistoryFailure.offline, .storage, .timeout, .cancelled, .failed] {
            XCTAssertEqual(HistoryClient.clearFailure(failure), .clearUncertain)
        }
        for failure in [HistoryFailure.busy, .unsupported] {
            XCTAssertEqual(HistoryClient.clearFailure(failure), failure)
        }
    }

    func testHistorySettingsPreservesExplicitFalse() throws {
        var patch = Aria_Daemon_V1_SettingsPatch()
        XCTAssertFalse(patch.hasHistoryEnabled)
        patch.historyEnabled = false
        let decoded = try Aria_Daemon_V1_SettingsPatch(serializedBytes: patch.serializedData())
        XCTAssertTrue(decoded.hasHistoryEnabled)
        XCTAssertFalse(decoded.historyEnabled)
    }
}
