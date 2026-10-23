import XCTest

@testable import Aria

final class SettingsHistoryTests: XCTestCase {
    func test_backAndForward_retraceTheVisitedPanes() {
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

    func test_open_afterGoingBack_dropsTheForwardPanes() {
        var history = SettingsHistory(current: .pane(.general))
        history.open(.pane(.dictation))
        history.goBack()

        history.open(.pane(.about))

        XCTAssertFalse(history.canGoForward)
        history.goBack()
        XCTAssertEqual(history.current, .pane(.general))
    }

    func test_open_theCurrentPane_recordsNothing() {
        var history = SettingsHistory(current: .pane(.general))

        history.open(.pane(.general))

        XCTAssertFalse(history.canGoBack)
    }

    func test_anAppPage_goesBackToTheAppsList() {
        var history = SettingsHistory(current: .pane(.apps))
        history.open(.item(.apps, id: "com.tinyspeck.slackmacgap"))

        XCTAssertEqual(history.current.pane, .apps)
        history.goBack()
        XCTAssertEqual(history.current, .pane(.apps))
        history.goForward()
        XCTAssertEqual(history.current, .item(.apps, id: "com.tinyspeck.slackmacgap"))
    }

    func test_goingBackFromAnItem_marksItInItsList() {
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
