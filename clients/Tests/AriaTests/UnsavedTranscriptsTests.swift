import XCTest

@testable import Aria

@MainActor
final class UnsavedTranscriptsTests: XCTestCase {
    /// A daemon and a file system that keep the unsaved transcripts in memory.
    @MainActor
    private final class Fake {
        var listed: [UnsavedTranscript]
        var isHistoryFull = false
        var transcriptsDir = URL(filePath: "/Users/me/.aria/transcripts", directoryHint: .isDirectory)
        var unsavedDir = URL(filePath: "/Users/me/.aria/unsaved", directoryHint: .isDirectory)
        var discardFailure: DaemonFailure?
        var copyFails = false
        var text: UnsavedTranscriptText = .whole("[00:00:01] heard")
        var readFails = false
        var holdsCopy = false
        var heldCopy: CheckedContinuation<Void, Never>?
        var lists = 0
        var listFails = false
        var copies: [(file: URL, destination: URL)] = []

        init(_ listed: [UnsavedTranscript]) { self.listed = listed }

        var effects: UnsavedTranscripts.Effects {
            .init(
                list: {
                    self.lists += 1
                    if self.listFails { throw DaemonFailure.refused(.historyUnavailable) }
                    return UnsavedTranscriptList(
                        transcripts: self.listed, isHistoryFull: self.isHistoryFull,
                        transcriptsDir: self.transcriptsDir, unsavedDir: self.unsavedDir)
                },
                discard: { transcript in
                    if let failure = self.discardFailure { throw failure }
                    self.listed.removeAll { $0.name == transcript.name }
                },
                read: { _ in
                    if self.readFails { throw DaemonFailure.refused(.historyMissing) }
                    return self.text
                },
                copy: { transcript, destination in
                    if self.holdsCopy { await withCheckedContinuation { self.heldCopy = $0 } }
                    if self.copyFails { throw CocoaError(.fileWriteNoPermission) }
                    self.copies.append((transcript.path, destination))
                })
        }
    }

    private static func transcript(_ name: String) -> UnsavedTranscript {
        UnsavedTranscript(
            name: name, path: URL(filePath: "/transcripts/.unsaved/\(name)"),
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

    func testSavingACopyCopiesTheFileThenDiscardsTheTranscript() async {
        let fake = Fake([Self.transcript("a.txt")])
        let unsaved = await refreshed(fake)
        let destination = URL(filePath: "/Users/me/Documents/a.txt")

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: destination)

        XCTAssertTrue(isDiscarded)
        XCTAssertEqual(fake.copies.map(\.file), [Self.transcript("a.txt").path])
        XCTAssertEqual(fake.copies.map(\.destination), [destination])
        XCTAssertEqual(unsaved.transcripts, [])
        XCTAssertEqual(fake.listed, [])
    }

    func testACopyThatFailsKeepsTheTranscriptAndDiscardsNothing() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.copyFails = true
        let unsaved = await refreshed(fake)

        let isDiscarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Volumes/Locked/a.txt"))

        XCTAssertFalse(isDiscarded)
        XCTAssertEqual(unsaved.transcripts.map(\.name), ["a.txt"])
        XCTAssertEqual(fake.listed.map(\.name), ["a.txt"])
        XCTAssertEqual(unsaved.error, "Aria couldn’t save the transcript there. Choose another place.")
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

    func testADiscardWhileASaveRunsSaysToWait() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.holdsCopy = true
        let unsaved = await refreshed(fake)
        let saving = Task { await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt")) }
        await waitUntil { fake.heldCopy != nil }

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, "Aria is still saving this transcript. Try again when it’s done.")
        fake.heldCopy?.resume()
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
        fake.readFails = true
        let gone = await unsaved.text(of: Self.transcript("a.txt"))

        XCTAssertEqual(cut, .truncated("[00:00:01] the start"))
        XCTAssertEqual(gone, .unreadable)
    }

    func testSavingACopyBesideTheTranscriptItselfIsRefused() async throws {
        let unsavedFolder = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: unsavedFolder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: unsavedFolder) }
        let file = unsavedFolder.appending(path: "a.txt")
        try Data("[00:00:01] heard".utf8).write(to: file)
        let transcript = UnsavedTranscript(
            name: "a.txt", path: file, startedAt: Date(timeIntervalSince1970: 100), duration: 60)
        let fake = Fake([transcript])
        fake.unsavedDir = unsavedFolder
        let unsaved = await refreshed(fake)
        let spelledAgain = unsavedFolder.appending(path: "../\(unsavedFolder.lastPathComponent)/a.txt")

        let discarded = await unsaved.save(transcript, to: spelledAgain)

        XCTAssertFalse(discarded)
        XCTAssertTrue(fake.copies.isEmpty)
        XCTAssertEqual(unsaved.error, "Choose a place outside Aria’s transcript folders.")
        XCTAssertEqual(unsaved.transcripts, [transcript])
    }

    func testSavingACopyIntoTheTranscriptsFolderOrBelowItIsRefusedHoweverItIsSpelled() async throws {
        let files = FileManager.default
        let home = files.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? files.removeItem(at: home) }
        let unsavedFolder = home.appending(path: ".aria/unsaved")
        let inner = home.appending(path: ".aria/transcripts/inner")
        try files.createDirectory(at: unsavedFolder, withIntermediateDirectories: true)
        try files.createDirectory(at: inner, withIntermediateDirectories: true)
        let alias = home.appending(path: "alias")
        try files.createSymbolicLink(at: alias, withDestinationURL: home.appending(path: ".aria/transcripts"))
        let file = unsavedFolder.appending(path: "a.txt")
        try Data("[00:00:01] heard".utf8).write(to: file)
        let transcript = UnsavedTranscript(
            name: "a.txt", path: file, startedAt: Date(timeIntervalSince1970: 100), duration: 60)
        let fake = Fake([transcript])
        fake.transcriptsDir = home.appending(path: ".aria/transcripts", directoryHint: .isDirectory)
        fake.unsavedDir = home.appending(path: ".aria/unsaved", directoryHint: .isDirectory)
        let unsaved = await refreshed(fake)

        for destination in [
            home.appending(path: ".aria/transcripts/a.txt"), inner.appending(path: "a.txt"),
            alias.appending(path: "inner/a.txt"),
        ] {
            let saved = await unsaved.save(transcript, to: destination)
            XCTAssertFalse(saved, destination.path)
        }
        XCTAssertTrue(fake.copies.isEmpty)
        XCTAssertEqual(unsaved.error, "Choose a place outside Aria’s transcript folders.")
        let savedOutside = await unsaved.save(transcript, to: home.appending(path: "a.txt"))
        XCTAssertTrue(savedOutside)
    }

    func testSavingACopyIntoTheFolderALinkedAriaFolderPointsToIsRefused() async throws {
        let files = FileManager.default
        let home = files.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? files.removeItem(at: home) }
        let unsavedTarget = home.appending(path: "Elsewhere/Unsaved")
        let transcriptsTarget = home.appending(path: "Elsewhere/Transcripts/inner")
        try files.createDirectory(at: unsavedTarget, withIntermediateDirectories: true)
        try files.createDirectory(at: transcriptsTarget, withIntermediateDirectories: true)
        try files.createDirectory(at: home.appending(path: ".aria"), withIntermediateDirectories: true)
        try files.createSymbolicLink(at: home.appending(path: ".aria/unsaved"), withDestinationURL: unsavedTarget)
        try files.createSymbolicLink(
            at: home.appending(path: ".aria/transcripts"),
            withDestinationURL: transcriptsTarget.deletingLastPathComponent())
        let file = home.appending(path: ".aria/unsaved/a.txt")
        try Data("[00:00:01] heard".utf8).write(to: file)
        let transcript = UnsavedTranscript(
            name: "a.txt", path: file, startedAt: Date(timeIntervalSince1970: 100), duration: 60)
        let fake = Fake([transcript])
        fake.transcriptsDir = home.appending(path: ".aria/transcripts", directoryHint: .isDirectory)
        fake.unsavedDir = home.appending(path: ".aria/unsaved", directoryHint: .isDirectory)
        let unsaved = await refreshed(fake)

        for destination in [
            unsavedTarget.appending(path: "a.txt"),
            transcriptsTarget.deletingLastPathComponent().appending(path: "a.txt"),
            transcriptsTarget.appending(path: "a.txt"),
        ] {
            let saved = await unsaved.save(transcript, to: destination)
            XCTAssertFalse(saved, destination.path)
        }
        XCTAssertTrue(fake.copies.isEmpty)
        XCTAssertEqual(unsaved.error, "Choose a place outside Aria’s transcript folders.")
        XCTAssertEqual(unsaved.transcripts, [transcript])
    }

    func testAReadOfAReplacedTranscriptRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.readFails = true
        let unsaved = await refreshed(fake)

        let text = await unsaved.text(of: Self.transcript("a.txt"))

        XCTAssertEqual(text, .unreadable)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testAnExportWhoseTranscriptChangedBeforeCleanupSaysSoAndRefreshes() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.historyMissing)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.save(Self.transcript("a.txt"), to: URL(filePath: "/Notes/a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(fake.copies.count, 1)
        XCTAssertEqual(unsaved.error, "Aria saved a copy. This transcript changed since, so Aria left it.")
        await waitUntil { fake.lists == 2 }
    }

    func testADiscardOfATranscriptAnotherFileReplacedRefreshesTheList() async {
        let fake = Fake([Self.transcript("a.txt")])
        fake.discardFailure = .refused(.historyMissing)
        let unsaved = await refreshed(fake)

        let discarded = await unsaved.discard(Self.transcript("a.txt"))

        XCTAssertFalse(discarded)
        XCTAssertEqual(unsaved.error, UnsavedTranscripts.goneNotice)
        await waitUntil { fake.lists == 2 }
    }

    func testACopyIsWholeAndStaysPrivate() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appending(path: UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? files.removeItem(at: folder) }
        let file = folder.appending(path: "a.txt")
        try Data("[00:00:01] heard".utf8).write(to: file)
        let transcript = UnsavedTranscript(
            name: "a.txt", path: file, startedAt: Date(timeIntervalSince1970: 100), duration: 60)
        let copied = folder.appending(path: "copied.txt")

        try await UnsavedTranscripts.Effects.copy(transcript, to: copied)

        XCTAssertEqual(try String(contentsOf: copied, encoding: .utf8), "[00:00:01] heard")
        XCTAssertEqual(try files.attributesOfItem(atPath: copied.path)[.posixPermissions] as? Int, 0o600)
    }

    func testACopyOfALinkCopiesNothing() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appending(path: UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? files.removeItem(at: folder) }
        let unrelated = folder.appending(path: "unrelated.txt")
        try Data("someone else’s".utf8).write(to: unrelated)
        let link = folder.appending(path: "a.txt")
        try files.createSymbolicLink(at: link, withDestinationURL: unrelated)
        let transcript = UnsavedTranscript(
            name: "a.txt", path: link, startedAt: Date(timeIntervalSince1970: 100), duration: 60)
        let copied = folder.appending(path: "copied.txt")

        do {
            try await UnsavedTranscripts.Effects.copy(transcript, to: copied)
            XCTFail("a link is copied")
        } catch {}

        XCTAssertFalse(files.fileExists(atPath: copied.path))
    }

    func testAListedTranscriptWithoutItsStartIsRefused() {
        var wire = Aria_Daemon_V1_UnsavedTranscript.with {
            $0.name = "a.txt"
            $0.path = "/Users/me/.aria/unsaved/a.txt"
        }

        XCTAssertThrowsError(try UnsavedTranscript(wire))
        wire.startedAt = .init(date: Date(timeIntervalSince1970: 100))
        XCTAssertEqual(try UnsavedTranscript(wire).name, "a.txt")
    }
}
