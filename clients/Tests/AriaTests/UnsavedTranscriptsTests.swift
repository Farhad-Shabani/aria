import XCTest

@testable import Aria

@MainActor
final class UnsavedTranscriptsTests: XCTestCase {
    /// A daemon and a file system that keep the unsaved transcripts in memory.
    @MainActor
    private final class Fake {
        var listed: [UnsavedTranscript]
        var isHistoryFull = false
        var unsavedDir = URL(filePath: "/Users/me/.aria/unsaved", directoryHint: .isDirectory)
        var discardFailure: DaemonFailure?
        var saveFailure: DaemonFailure?
        var isKept = false
        var text: UnsavedTranscriptText = .whole("[00:00:01] heard")
        var readFailure: DaemonFailure?
        var holdsSave = false
        var heldSave: CheckedContinuation<Void, Never>?
        var lists = 0
        var listFails = false
        var saves: [(name: String, destination: URL)] = []

        init(_ listed: [UnsavedTranscript]) { self.listed = listed }

        var effects: UnsavedTranscripts.Effects {
            .init(
                list: {
                    self.lists += 1
                    if self.listFails { throw DaemonFailure.refused(.historyUnavailable) }
                    return UnsavedTranscriptList(
                        transcripts: self.listed, isHistoryFull: self.isHistoryFull, unsavedDir: self.unsavedDir)
                },
                discard: { transcript in
                    if let failure = self.discardFailure { throw failure }
                    self.listed.removeAll { $0.name == transcript.name }
                },
                read: { _ in
                    if let failure = self.readFailure { throw failure }
                    return self.text
                },
                save: { transcript, destination in
                    if self.holdsSave { await withCheckedContinuation { self.heldSave = $0 } }
                    if let failure = self.saveFailure { throw failure }
                    self.saves.append((transcript.name, destination))
                    if !self.isKept { self.listed.removeAll { $0.name == transcript.name } }
                    return self.isKept
                })
        }
    }

    private static func transcript(_ name: String) -> UnsavedTranscript {
        UnsavedTranscript(
            name: name, path: URL(filePath: "/Users/me/.aria/unsaved/\(name)"),
            startedAt: Date(timeIntervalSince1970: 100), duration: 60)
    }

    private func refreshed(_ fake: Fake) async -> UnsavedTranscripts {
        let unsaved = UnsavedTranscripts(effects: fake.effects)
        await unsaved.refresh()
        return unsaved
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    func testRefreshingListsTheTranscriptsAndWhetherHistoryIsFull() async {
        let fake = Fake([Self.transcript("a.txt"), Self.transcript("b.txt")])
        fake.isHistoryFull = true

        let unsaved = await refreshed(fake)

        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt", "b.txt"])
        XCTAssertTrue(unsaved.isHistoryFull)
    }

    func testAListThatFailsKeepsTheTranscriptsAndSaysSoUntilOneComesBack() async {
        let fake = Fake([Self.transcript("a.txt")])
        let unsaved = await refreshed(fake)
        fake.listFails = true

        await unsaved.refresh()
        let failure = unsaved.listFailure
        fake.listFails = false
        await unsaved.refresh()

        XCTAssertEqual(failure, "Aria couldn’t list your unsaved transcripts. Try again.")
        XCTAssertNil(unsaved.listFailure)
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
    }

    func testADiscardHistoryCantCheckYetKeepsTheTranscriptAndSaysWhy() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.historyUnavailable)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, "Aria can’t discard this transcript until history opens. Try again later.")
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
    }

    func testOnlyASessionEndingRefreshesTheList() async {
        let fake = Fake([])
        let unsaved = UnsavedTranscripts(effects: fake.effects)

        unsaved.handle(.with { $0.stage = .started })
        unsaved.handle(.with { $0.stage = .segmentTranscribed })
        unsaved.handle(nil)
        unsaved.handle(.with { $0.stage = .stopped })
        unsaved.handle(.with { $0.stage = .failed })
        await waitUntil { fake.lists == 2 }

        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(fake.lists, 2)
    }

    func testSavingSavesThroughAriaThenDropsTheRow() async {
        let fake = Fake([Self.transcript("a.txt")])
        let unsaved = await refreshed(fake)
        let destination = URL(filePath: "/Users/me/Documents/a.txt")

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: destination)

        XCTAssertTrue(isDiscarded)
        XCTAssertEqual(fake.saves.map(\.name), ["a.txt"])
        XCTAssertEqual(fake.saves.map(\.destination), [destination])
        XCTAssertEqual(unsaved.transcripts, [])
        XCTAssertNil(unsaved.error)
    }

    func testASaveAriaKeptSaysTheCopyIsMadeAndRefreshes() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.isKept = true
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(
            unsaved.error, "Aria saved a copy but couldn’t remove this one. Click **Discard** to remove it.")
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
        await waitUntil { fake.lists == 2 }
    }

    func testASaveAriaRefusesKeepsTheTranscriptAndSaysWhy() async {
        for (failure, message) in [
            (DaemonFailure.refused(.historyUnavailable), "Aria can’t save this transcript right now. Try again later."),
            (.refused(.invalidRequest), "Aria uses that file for its own transcripts. Choose another place."),
            (.unavailable, DaemonFailure.offlineMessage),
            (.timeout, "Aria couldn’t save the transcript there. Choose another place."),
        ] {
            let fake = Fake([Self.transcript("a.txt")])
            fake.saveFailure = failure
            let unsaved = await refreshed(fake)

            let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

            XCTAssertFalse(isDiscarded, message)
            XCTAssertEqual(unsaved.error, message)
            XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"], message)
        }
    }

    func testASaveOfAGoneTranscriptRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.saveFailure = .refused(.unsavedTranscriptMissing)
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testADiscardWhileASaveRunsSaysToWait() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.holdsSave = true
        let unsaved = await refreshed(fake)
        let saving = Task { await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt")) }
        await waitUntil { fake.heldSave != nil }

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, "Aria is still saving this transcript. Try again when it’s done.")
        fake.heldSave?.resume()
        _ = await saving.value
    }

    func testADiscardThatFailsKeepsTheTranscript() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .unavailable
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
        XCTAssertEqual(unsaved.error, DaemonFailure.offlineMessage)
    }

    func testTheNextChangeClearsTheLastError() async {
        let fake = Fake([Self.transcript("a.txt"), Self.transcript("b.txt")])
        fake.discardFailure = .timeout
        let unsaved = await refreshed(fake)
        await unsaved.discard(Self.transcript("a.txt"))
        XCTAssertNotNil(unsaved.error)

        fake.discardFailure = nil
        await unsaved.discard(Self.transcript("b.txt"))

        XCTAssertNil(unsaved.error)
    }

    func testTheTextIsWhatTheDaemonReadsAndUnreadableWhenItCant() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.text = .truncated("[00:00:01] the start")
        let unsaved = await refreshed(fake)

        let cut = await unsaved.text(of: Self.transcript("a.txt"))
        fake.readFailure = .refused(.unsavedTranscriptMissing)
        let gone = await unsaved.text(of: Self.transcript("a.txt"))

        XCTAssertEqual(cut, .truncated("[00:00:01] the start"))
        XCTAssertEqual(gone, .unreadable)
    }

    func testAReadWhileAriaIsOffSaysSoAndKeepsTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.readFailure = .unavailable
        let unsaved = await refreshed(fake)

        let text = await unsaved.text(of: Self.transcript("a.txt"))

        XCTAssertEqual(text, .unavailable)
        XCTAssertNil(unsaved.error)
        XCTAssertEqual(fake.lists, 1)
    }

    func testAListThePageCancelledLeavesNoFailure() async {
        let fake = Fake([Self.transcript("a.txt")])
        let unsaved = await refreshed(fake)
        fake.listFails = true

        let refresh = Task { await unsaved.refresh() }
        refresh.cancel()
        await refresh.value

        XCTAssertNil(unsaved.listFailure)
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
    }

    func testAReadOfAReplacedTranscriptRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.readFailure = .refused(.unsavedTranscriptMissing)
        let unsaved = await refreshed(fake)

        let text = await unsaved.text(of: Self.transcript("a.txt"))

        XCTAssertEqual(text, .unreadable)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testADiscardOfATranscriptAnotherFileReplacedRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.unsavedTranscriptMissing)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testAListedTranscriptWithoutItsStartIsRefused() {
        let folder = URL(filePath: "/Users/me/.aria/unsaved", directoryHint: .isDirectory)
        var wire = Aria_Daemon_V1_UnsavedTranscript.with { $0.name = "a.txt" }

        XCTAssertThrowsError(try UnsavedTranscript(wire, in: folder))
        wire.startedAt = .init(date: Date(timeIntervalSince1970: 100))
        XCTAssertEqual(try UnsavedTranscript(wire, in: folder).name, "a.txt")
    }

    func testAListedTranscriptLivesInTheFolderAndNamesOnlyAFile() throws {
        let folder = URL(filePath: "/Users/me/.aria/unsaved", directoryHint: .isDirectory)
        var wire = Aria_Daemon_V1_UnsavedTranscript.with {
            $0.startedAt = .init(date: Date(timeIntervalSince1970: 100))
        }

        wire.name = "a.txt"
        XCTAssertEqual(try UnsavedTranscript(wire, in: folder).path.path, "/Users/me/.aria/unsaved/a.txt")
        for refused in ["", ".", "..", "../a.txt", "a/b.txt"] {
            wire.name = refused
            XCTAssertThrowsError(try UnsavedTranscript(wire, in: folder), refused)
        }
    }
}
