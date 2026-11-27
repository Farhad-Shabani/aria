import XCTest

@testable import Aria

final class PaletteUnsavedTranscriptTests: XCTestCase {
    private static let liveFile = URL(filePath: "/transcripts/live.txt")

    private static func transcript(_ name: String) -> UnsavedTranscript {
        UnsavedTranscript(
            name: name, path: URL(filePath: "/transcripts/.unsaved/\(name)"),
            startedAt: Date(timeIntervalSince1970: 100), duration: 60)
    }

    private static func entry(_ id: UInt64, transcript: String? = nil) -> HistoryItem {
        var entry = HistoryItem(id: id, created: Date(timeIntervalSince1970: 200), text: "", status: .success)
        if let transcript { entry.kind = .systemTranscription(transcript: transcript, read: nil) }
        return entry
    }

    private func openModel(
        unsaved: [UnsavedTranscript], history: [HistoryItem]? = nil, opening shown: UnsavedTranscript? = nil
    ) -> PaletteModel {
        var model = PaletteModel()
        model.reset(prompts: [], isSystemTranscriptionRunning: false)
        if let history {
            model.keepsHistory = true
            model.replace(entries: history)
        }
        model.replace(unsaved: unsaved)
        if let shown { model.open(unsaved: shown) }
        return model
    }

    func testUnsavedTranscriptsArePinnedFirstWithOrWithoutHistory() {
        let unsaved = [Self.transcript("a.txt"), Self.transcript("b.txt")]
        let withoutHistory = openModel(unsaved: unsaved)
        let withHistory = openModel(unsaved: unsaved, history: [Self.entry(1)])

        for model in [withoutHistory, withHistory] {
            XCTAssertEqual(model.items.prefix(2), [.unsaved(unsaved[0]), .unsaved(unsaved[1])])
            XCTAssertEqual(model.selected, .unsaved(unsaved[0]))
        }
        XCTAssertEqual(withoutHistory.items[2].command?.action, .startTranscription)
        XCTAssertEqual(withHistory.items.dropFirst(2).map(\.id), ["entry-1"])
    }

    func testARunningSessionsRowStaysAboveTheUnsavedOnes() {
        let unsaved = Self.transcript("a.txt")
        var withoutHistory = openModel(unsaved: [unsaved])
        var withHistory = openModel(unsaved: [unsaved], history: [Self.entry(1)])

        withoutHistory.isSystemTranscriptionRunning = true
        withHistory.isSystemTranscriptionRunning = true

        for model in [withoutHistory, withHistory] {
            XCTAssertEqual(model.items.first?.command?.action, .showLiveTranscript)
            XCTAssertEqual(model.items[1], .unsaved(unsaved))
            XCTAssertEqual(model.items.filter { $0.command?.action == .showLiveTranscript }.count, 1)
        }
    }

    func testASearchKeepsTheUnsavedRowsOnlyWhenItMatchesThem() {
        var model = openModel(unsaved: [Self.transcript("a.txt")])

        model.query = "unsaved"
        XCTAssertEqual(model.items.first, .unsaved(Self.transcript("a.txt")))

        model.query = "settings"
        XCTAssertFalse(model.items.contains(.unsaved(Self.transcript("a.txt"))))
    }

    func testTheUnsavedPageOffersSaveAndDiscardWhateverHistoryKeeps() {
        let transcript = Self.transcript("a.txt")
        for keepsHistory in [false, true] {
            var model = openModel(unsaved: [transcript], opening: transcript)
            model.keepsHistory = keepsHistory

            let actions = PaletteActions(model: model).all

            XCTAssertEqual(actions.map(\.kind), [.saveTranscript(transcript), .discardTranscript(transcript)])
            XCTAssertTrue(actions.allSatisfy { $0.isInFooter && $0.isEnabled })
            XCTAssertEqual(actions.map(\.title), ["Save…", "Discard…"])
            XCTAssertEqual(actions.first?.key.label, "⌘S")
        }
    }

    func testEveryUnsavedActionWaitsOnAChange() {
        let transcript = Self.transcript("a.txt")
        let model = openModel(unsaved: [transcript], opening: transcript)

        let changing = PaletteActions(model: model, isChangingUnsaved: true).all

        XCTAssertEqual(changing.map(\.isEnabled), [false, false])
    }

    func testEscapeOrTheShortcutOnTheUnsavedPageAsksBeforeLeaving() {
        let transcript = Self.transcript("a.txt")
        var model = openModel(unsaved: [transcript], opening: transcript)

        XCTAssertFalse(model.escape())
        model.isConfirmingTranscriptDiscard = false
        XCTAssertFalse(model.closeRequested())

        XCTAssertTrue(model.isConfirmingTranscriptDiscard)
        XCTAssertEqual(model.page, .unsavedTranscript(transcript))
    }

    func testTheShortcutClosesAnyOtherPageAtOnce() {
        var model = openModel(unsaved: [Self.transcript("a.txt")])

        XCTAssertTrue(model.closeRequested())
        XCTAssertFalse(model.isConfirmingTranscriptDiscard)
    }

    func testAPageWhoseTranscriptLeftTheListGoesBackToTheCommands() {
        let kept = Self.transcript("b.txt")
        var model = openModel(unsaved: [Self.transcript("a.txt"), kept])
        model.open(unsaved: Self.transcript("a.txt"))

        model.replace(unsaved: [kept])

        XCTAssertEqual(model.page, .commands)
        XCTAssertEqual(model.items.first, .unsaved(kept))
    }

    func testTheLivePageMovesToTheTranscriptItsEndedSessionLeftUnsaved() {
        let ended = Self.transcript("ended.txt")
        var model = openModel(unsaved: [])
        model.isSystemTranscriptionRunning = true
        model.transcriptFile = ended.path
        model.show(.liveTranscript)

        model.replace(unsaved: [ended])
        XCTAssertEqual(model.page, .liveTranscript, "A running session stays live")

        model.isSystemTranscriptionRunning = false
        model.replace(unsaved: [Self.transcript("other.txt")])
        XCTAssertEqual(model.page, .liveTranscript, "Another file isn't this session's")

        model.replace(unsaved: [Self.transcript("other.txt"), ended])
        XCTAssertEqual(model.page, .unsavedTranscript(ended))
    }

    func testOnlyTheRunningSessionsEntryOpensTheLivePageAndReadsAsTranscribing() {
        var model = openModel(unsaved: [])
        model.transcriptFile = Self.liveFile
        let live = Self.entry(1, transcript: Self.liveFile.path(percentEncoded: false))
        var named = live
        named.title = "Standup"
        let other = Self.entry(2, transcript: "/transcripts/old.txt")
        XCTAssertFalse(model.isLive(live), "No session runs")

        model.isSystemTranscriptionRunning = true

        XCTAssertTrue(model.isLive(live))
        XCTAssertFalse(model.isLive(other))
        XCTAssertFalse(model.isLive(Self.entry(3)))
        XCTAssertEqual(model.liveName(of: live), "Transcribing…")
        XCTAssertNil(model.liveName(of: named))
        XCTAssertNil(model.liveName(of: other))
    }

    func testShowInFinderIsOfferedOnlyForMacAudioEntries() {
        let transcriptPath = "/transcripts/standup.txt"
        let macAudio = Self.entry(1, transcript: transcriptPath)
        var dictation = Self.entry(2)
        dictation.text = "Hello"
        var model = openModel(unsaved: [], history: [macAudio, dictation])
        model.open(entry: macAudio.id)

        let onPage = PaletteActions(model: model, detail: macAudio).all.map(\.kind)
        let onDictation = PaletteActions(model: model, detail: dictation).all.map(\.kind)

        XCTAssertTrue(onPage.contains(.showInFinder(URL(filePath: transcriptPath))))
        XCTAssertFalse(onDictation.contains { if case .showInFinder = $0 { true } else { false } })
    }
}
