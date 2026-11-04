import XCTest

@testable import Aria

@MainActor
final class DictationHistoryTests: XCTestCase {
    @MainActor
    private final class Harness {
        var entries = [item(2), item(1)]
        var deletionFails = false
        var copied = [String]()
        var copySucceeds = true
        var regenerateContinuation: CheckedContinuation<HistoryItem, Error>?
        var clearCalls = 0
        var clearFailure: HistoryFailure?
        var isCleanupPending = false

        var effects: DictationHistoryState.Effects {
            .init(
                list: { _, _ in HistoryPage(items: self.entries) },
                get: { id in
                    guard let entry = self.entries.first(where: { $0.id == id }) else {
                        throw HistoryFailure.missing
                    }
                    return entry
                },
                delete: { id in
                    if self.deletionFails { throw HistoryFailure.storage }
                    self.entries.removeAll { $0.id == id }
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

    private static func item(_ id: UInt64) -> HistoryItem {
        HistoryItem(id: id, text: "Text \(id)", created: Date(timeIntervalSince1970: 100), status: .success)
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    func testRefreshPreservesSelectionAndFetchesFullText() async {
        let harness = Harness()
        let state = DictationHistoryState(effects: harness.effects)
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
        effects.list = { _, _ in
            lists += 1
            return HistoryPage(items: harness.entries)
        }
        let state = DictationHistoryState(effects: effects)

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

    func testDaysGroupNewestFirstAndKeepOrderWithinADay() {
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = .gmt
        let day: TimeInterval = 86_400
        func entry(_ id: UInt64, at seconds: TimeInterval) -> HistoryItem {
            HistoryItem(id: id, text: "", created: Date(timeIntervalSince1970: seconds), status: .success)
        }
        let items = [entry(3, at: day + 50), entry(2, at: 20), entry(1, at: 10)]

        let days = HistoryDay.grouping(items, calendar: utc)

        XCTAssertEqual(days.map(\.start), [Date(timeIntervalSince1970: day), Date(timeIntervalSince1970: 0)])
        XCTAssertEqual(days.map { $0.items.map(\.id) }, [[3], [2, 1]])
    }

    func testDaysFollowTheListedItems() async {
        let harness = Harness()
        let state = DictationHistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { !state.items.isEmpty }
        XCTAssertEqual(state.days, HistoryDay.grouping(state.items))
        state.close()
    }

    func testSupersededSearchCannotReplaceNewResults() async {
        let harness = Harness()
        var continuations: [CheckedContinuation<HistoryPage, Error>] = []
        var effects = harness.effects
        effects.list = { _, _ in try await withCheckedThrowingContinuation { continuations.append($0) } }
        let state = DictationHistoryState(effects: effects)
        state.refresh()
        await waitUntil { continuations.count == 1 }
        state.search("new")
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
        effects.list = { _, before in
            before == nil
                ? HistoryPage(items: [harness.entries[0]], next: 2)
                : HistoryPage(items: [harness.entries[1]])
        }
        let state = DictationHistoryState(effects: effects)
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

    func testSupersededSelectionCannotReplaceCurrentDetail() async {
        let harness = Harness()
        var reads: [UInt64: CheckedContinuation<HistoryItem, Error>] = [:]
        var effects = harness.effects
        effects.get = { id in try await withCheckedThrowingContinuation { reads[id] = $0 } }
        let state = DictationHistoryState(effects: effects)
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
        let state = DictationHistoryState(effects: harness.effects)
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
        let state = DictationHistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.delete(2)
        await waitUntil { state.detail?.id == 1 }
        XCTAssertEqual(state.items.map(\.id), [1])
        state.close()
    }

    func testRetryAfterSelectionChangeUpdatesOwnRowWithoutCopy() async {
        let harness = Harness()
        harness.entries[0].canRegenerate = true
        harness.entries[0].status = .transcriptionFailed
        let state = DictationHistoryState(effects: harness.effects)
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
        let state = DictationHistoryState(effects: effects)
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
        let state = DictationHistoryState(effects: effects)
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
        let state = DictationHistoryState(effects: effects)
        state.refresh()
        await waitUntil { poll != nil }
        state.close()
        poll?.resume()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(reads, 1)
    }

    func testFailedClipboardWriteDoesNotClaimCopied() async {
        let harness = Harness()
        harness.copySucceeds = false
        let state = DictationHistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        state.copy()
        XCTAssertEqual(harness.copied, ["Text 2"])
        XCTAssertEqual(state.notice, "Aria couldn’t copy the text. Try again.")
        state.close()
    }

    func testClearResetsSearchPagesAndSelectionAndPreservesCleanupNotice() async {
        let harness = Harness()
        harness.isCleanupPending = true
        var effects = harness.effects
        var requests: [(String, UInt64?)] = []
        var warning: String?
        effects.list = { query, before in
            requests.append((query, before))
            guard !harness.entries.isEmpty else { return HistoryPage(items: [], warning: warning) }
            return before == nil
                ? HistoryPage(items: [harness.entries[0]], next: 2)
                : HistoryPage(items: [harness.entries[1]])
        }
        let state = DictationHistoryState(effects: effects)
        state.search("Text")
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
        XCTAssertEqual(state.query, "")
        XCTAssertEqual(requests.last?.0, "")
        XCTAssertNil(requests.last?.1)
        let notice = "Dictation text cleared. Aria will finish removing some recordings later."
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
        effects.list = { _, _ in try await withCheckedThrowingContinuation { lists.append($0) } }
        effects.get = { _ in try await withCheckedThrowingContinuation { read = $0 } }
        effects.clear = { try await withCheckedThrowingContinuation { clear = $0 } }
        let state = DictationHistoryState(effects: effects)
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
        state.search("ignored")
        state.refresh()
        state.loadMore(after: 1)
        state.select(1)
        XCTAssertTrue(state.isClearing)
        XCTAssertFalse(state.canClear)
        XCTAssertNil(state.deletingID)
        XCTAssertNil(state.regeneratingID)
        XCTAssertEqual(state.query, "")
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
            let state = DictationHistoryState(effects: harness.effects)
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
        let state = DictationHistoryState(effects: harness.effects)
        state.refresh()
        await waitUntil { state.detail != nil }
        XCTAssertFalse(state.canClear)
        state.clear()
        XCTAssertFalse(state.isClearing)
        XCTAssertEqual(harness.clearCalls, 0)
        state.close()
    }

    func testCloseDuringClearReleasesControlsAndIgnoresLateCompletion() async {
        let harness = Harness()
        var clears: [CheckedContinuation<HistoryClearResult, Error>] = []
        var effects = harness.effects
        effects.clear = { try await withCheckedThrowingContinuation { clears.append($0) } }
        let state = DictationHistoryState(effects: effects)
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

    func testMetadataOmitsUnknownApplicationDurationAndWords() throws {
        var wire = Aria_Daemon_V1_HistoryEntry()
        wire.id = 1
        wire.source = .relay
        wire.status = .transcriptionFailed
        wire.audioAvailability = .unavailable
        let item = try HistoryItem(wire)
        XCTAssertEqual(item.metadata.map(\.0), ["Source", "Status", "Created", "Audio"])
        XCTAssertEqual(item.source, .relay)
        XCTAssertEqual(item.source.title, "iPhone or Apple Watch")
        XCTAssertFalse(item.canRegenerate)
    }

    func testTranscriptionErrorShowsExplanationInsteadOfRawCategory() throws {
        var wire = Aria_Daemon_V1_HistoryEntry()
        wire.id = 1
        wire.source = .local
        wire.status = .transcriptionFailed
        wire.audioAvailability = .unavailable
        wire.error.code = .transcriptionFailed
        XCTAssertEqual(
            try HistoryItem(wire).message, "Aria couldn’t finish transcribing this recording.")
        wire.error.code = .UNRECOGNIZED(12345)
        XCTAssertEqual(
            try HistoryItem(wire).message, "Aria couldn’t finish transcribing this recording.")
        wire.error.code = .overallTimeout
        XCTAssertEqual(try HistoryItem(wire).message, "Aria couldn’t finish this recording in time.")
    }

    func testAudioExpiryMatchesAvailability() throws {
        var wire = Aria_Daemon_V1_HistoryEntry()
        wire.id = 1
        wire.source = .local
        wire.status = .transcriptionFailed
        wire.audioAvailability = .available
        XCTAssertThrowsError(try HistoryItem(wire))
        wire.audioExpiresAtMs = 1_000
        let expiry = Date(timeIntervalSince1970: 1)
        let available = try HistoryItem(wire)
        XCTAssertEqual(available.audio, .available(until: expiry))
        XCTAssertEqual(
            available.metadata.first { $0.0 == "Audio" }?.1,
            "Available until \(expiry.formatted(date: .abbreviated, time: .shortened))")
        var successful = available
        successful.status = .success
        XCTAssertFalse(successful.metadata.contains { $0.0 == "Audio" })
        let cases: [(Aria_Daemon_V1_HistoryAudioAvailability, HistoryItem.Audio, String)] = [
            (.unavailable, .unavailable, "Unavailable"),
            (.expired, .expired, "Expired"),
        ]
        for (audio, expected, description) in cases {
            wire.audioAvailability = audio
            XCTAssertThrowsError(try HistoryItem(wire))
            wire.clearAudioExpiresAtMs()
            let item = try HistoryItem(wire)
            XCTAssertEqual(item.audio, expected)
            XCTAssertEqual(item.metadata.first { $0.0 == "Audio" }?.1, description)
            wire.audioExpiresAtMs = 1_000
        }
        wire.clearAudioExpiresAtMs()
        for audio in [Aria_Daemon_V1_HistoryAudioAvailability.unspecified, .UNRECOGNIZED(12345)] {
            wire.audioAvailability = audio
            XCTAssertThrowsError(try HistoryItem(wire))
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
