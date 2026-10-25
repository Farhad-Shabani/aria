import XCTest

@testable import Aria

final class SettingsHistoryTests: XCTestCase {
    func testBackAndForwardRetraceTheVisitedPanes() {
        var history = SettingsHistory(current: .pane(.general))
        history.open(.pane(.dictation))
        history.open(.pane(.phone))

        history.goBack()
        XCTAssertEqual(history.current, .pane(.dictation))
        history.goBack()
        XCTAssertEqual(history.current, .pane(.general))
        XCTAssertFalse(history.canGoBack)
        history.goForward()
        XCTAssertEqual(history.current, .pane(.dictation))
        XCTAssertTrue(history.canGoForward)
    }

    func testOpenAfterGoingBackDropsTheForwardPanes() {
        var history = SettingsHistory(current: .pane(.general))
        history.open(.pane(.dictation))
        history.goBack()

        history.open(.pane(.about))

        XCTAssertFalse(history.canGoForward)
        history.goBack()
        XCTAssertEqual(history.current, .pane(.general))
    }

    func testOpenTheCurrentPaneRecordsNothing() {
        var history = SettingsHistory(current: .pane(.general))

        history.open(.pane(.general))

        XCTAssertFalse(history.canGoBack)
    }

    func testAnAppPageGoesBackToTheAppsList() {
        var history = SettingsHistory(current: .pane(.apps))
        history.open(.item(.apps, id: "com.tinyspeck.slackmacgap"))

        XCTAssertEqual(history.current.pane, .apps)
        history.goBack()
        XCTAssertEqual(history.current, .pane(.apps))
        history.goForward()
        XCTAssertEqual(history.current, .item(.apps, id: "com.tinyspeck.slackmacgap"))
    }

    func testGoingBackFromAnItemMarksItInItsList() {
        var history = SettingsHistory(current: .pane(.apps))
        history.open(.item(.apps, id: "com.a"))
        XCTAssertNil(history.lastItem(in: .apps))

        history.goBack()

        XCTAssertEqual(history.lastItem(in: .apps), "com.a")
        XCTAssertNil(history.lastItem(in: .privacy))
        history.open(.pane(.privacy))
        XCTAssertNil(history.lastItem(in: .apps))
    }
}
