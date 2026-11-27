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
        var exportFailure: Error?
        var text: UnsavedTranscriptText = .whole("[00:00:01] heard")
        var readFailure: DaemonFailure?
        var holdsExport = false
        var heldExport: CheckedContinuation<Void, Never>?
        var lists = 0
        var listFails = false
        var exports: [(name: String, destination: URL)] = []

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
                export: { transcript, destination in
                    if self.holdsExport { await withCheckedContinuation { self.heldExport = $0 } }
                    if let failure = self.exportFailure { throw failure }
                    self.exports.append((transcript.name, destination))
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

    func testSavingACopiesTheFileThenDiscardsTheTranscript() async {
        let fake = Fake([Self.transcript("a.txt")])
        let unsaved = await refreshed(fake)
        let destination = URL(filePath: "/Users/me/Documents/a.txt")

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: destination)

        XCTAssertTrue(isDiscarded)
        XCTAssertEqual(fake.exports.map(\.name), ["a.txt"])
        XCTAssertEqual(fake.exports.map(\.destination), [destination])
        XCTAssertEqual(unsaved.transcripts, [])
        XCTAssertEqual(fake.listed, [])
    }

    func testACopyAriaRefusesKeepsTheTranscriptAndSaysWhy() async {
        for (failure, message) in [
            (
                DaemonFailure.refused(.invalidRequest),
                "Aria uses that file for its own transcripts. Choose another place."
            ),
            (.refused(.busy), "Aria is writing to that file now. Choose another place."),
            (.refused(.historyUnavailable), "Aria can’t save this transcript right now. Try again later."),
            (.unavailable, DaemonFailure.offlineMessage),
            (.timeout, "Aria couldn’t save the transcript there. Choose another place."),
        ] {
            let fake = Fake([Self.transcript("a.txt")])
            fake.exportFailure = failure
            let unsaved = await refreshed(fake)

            let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

            XCTAssertFalse(isDiscarded, message)
            XCTAssertEqual(unsaved.error, message)
            XCTAssertEqual(fake.listed.map(\.name), ["a.txt"], message)
        }
    }

    func testACopyThatFailsToWriteKeepsTheTranscriptAndDiscardsNothing() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.exportFailure = CocoaError(.fileWriteNoPermission)
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Volumes/Locked/a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(fake.listed.map(\.name), ["a.txt"])
        XCTAssertEqual(unsaved.error, "Aria couldn’t save the transcript there. Choose another place.")
    }

    func testACopyOfAGoneTranscriptRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.exportFailure = DaemonFailure.refused(.unsavedTranscriptMissing)
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testACopyWhoseDiscardFailsSaysTheCopyIsMadeEvenWithAriaOff() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .unavailable
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(
            unsaved.error,
            "Aria saved a copy but couldn’t remove this one. Click **Discard** to remove it. "
                + DaemonFailure.offlineMessage)
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
    }

    func testACopyWhoseDiscardWaitsForHistorySaysSoInsteadOfOfferingDiscard() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.historyUnavailable)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, "Aria saved a copy. Click **Discard** to remove this one once history opens.")
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
    }

    func testACopyWhoseTranscriptChangedBeforeCleanupSaysSoAndRefreshes() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.unsavedTranscriptMissing)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(fake.exports.count, 1)
        XCTAssertEqual(unsaved.error, "Aria saved a copy. This transcript changed since, so Aria left it.")
        await waitUntil { fake.lists == 2 }
    }

    func testANewCopyIsWholeAndPrivateAndASecondReplacesIt() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appending(path: UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? files.removeItem(at: folder) }
        let destination = folder.appending(path: "a.txt")

        try await UnsavedTranscripts.Effects.write(to: destination) { receive in
            try receive(Data("[00:00:01] first ".utf8))
            try receive(Data("[00:00:02] second".utf8))
        }
        let first = try String(contentsOf: destination, encoding: .utf8)
        let mode = try files.attributesOfItem(atPath: destination.path)[.posixPermissions] as? Int
        try await UnsavedTranscripts.Effects.write(to: destination) { receive in
            try receive(Data("[00:00:01] again".utf8))
        }

        XCTAssertEqual(first, "[00:00:01] first [00:00:02] second")
        XCTAssertEqual(mode, 0o600)
        XCTAssertEqual(try String(contentsOf: destination, encoding: .utf8), "[00:00:01] again")
    }

    func testAnExportThatFailsPartwayLeavesNoFile() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appending(path: UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? files.removeItem(at: folder) }
        let destination = folder.appending(path: "a.txt")

        do {
            try await UnsavedTranscripts.Effects.write(to: destination) { receive in
                try receive(Data("[00:00:01] first".utf8))
                throw DaemonFailure.timeout
            }
            XCTFail("a partial export was written")
        } catch {}

        XCTAssertFalse(files.fileExists(atPath: destination.path))
    }

    func testADiscardWhileASaveRunsSaysToWait() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.holdsExport = true
        let unsaved = await refreshed(fake)
        let saving = Task { await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt")) }
        await waitUntil { fake.heldExport != nil }

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, "Aria is still saving this transcript. Try again when it’s done.")
        fake.heldExport?.resume()
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
