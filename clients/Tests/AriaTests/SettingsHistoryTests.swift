import XCTest

@testable import Aria

final class SettingsHistoryTests: XCTestCase {
    func test_backAndForward_retraceTheVisitedPanes() {
        var history = SettingsHistory(current: .general)
        history.open(.dictation)
        history.open(.phone)

        history.goBack()
        XCTAssertEqual(history.current, .dictation)
        history.goBack()
        XCTAssertEqual(history.current, .general)
        XCTAssertFalse(history.canGoBack)
        history.goForward()
        XCTAssertEqual(history.current, .dictation)
        XCTAssertTrue(history.canGoForward)
    }

    func test_open_afterGoingBack_dropsTheForwardPanes() {
        var history = SettingsHistory(current: .general)
        history.open(.dictation)
        history.goBack()

        history.open(.about)

        XCTAssertFalse(history.canGoForward)
        history.goBack()
        XCTAssertEqual(history.current, .general)
    }

    func test_open_theCurrentPane_recordsNothing() {
        var history = SettingsHistory(current: .general)

        history.open(.general)

        XCTAssertFalse(history.canGoBack)
    }
}
