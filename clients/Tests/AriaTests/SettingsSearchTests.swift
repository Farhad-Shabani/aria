import XCTest

@testable import Aria

final class SettingsSearchTests: XCTestCase {
    func testAQueryMatchesARowTitleIgnoringCase() {
        XCTAssertEqual(
            SettingsSearch.results(for: "MUTE"),
            [SettingsSearch.Result(pane: .dictation, rows: [.muteWhileRecording])])
    }

    func testAKeywordFindsARowItsTitleDoesNotName() {
        XCTAssertEqual(
            SettingsSearch.results(for: "silence"),
            [SettingsSearch.Result(pane: .dictation, rows: [.autoStop])])
    }

    func testAPaneTitleMatchListsThatPaneWithNoRows() {
        XCTAssertEqual(
            SettingsSearch.results(for: "about"),
            [SettingsSearch.Result(pane: .about, rows: [])])
    }

    func testABlankQueryIsNoSearch() {
        XCTAssertNil(SettingsSearch.results(for: "  "))
    }

    func testAQueryMatchingNothingFindsNoPanes() {
        XCTAssertEqual(SettingsSearch.results(for: "xyz"), [])
    }

    func testResultsFollowSidebarOrderAndPageOrder() {
        XCTAssertEqual(
            SettingsSearch.results(for: "Recording"),
            [
                SettingsSearch.Result(
                    pane: .dictation, rows: [.microphone, .muteWhileRecording, .returnSends, .autoStop]),
                SettingsSearch.Result(pane: .privacy, rows: [.keepAudio]),
            ])
    }
}
