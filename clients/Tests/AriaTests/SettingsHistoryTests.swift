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

    func testOpeningAnItemSelectsItInItsList() {
        var history = SettingsHistory(current: .pane(.apps))
        history.open(.item(.apps, id: "com.a"))
        XCTAssertEqual(history.selectedItem(in: .apps), "com.a")

        history.goBack()

        XCTAssertEqual(history.selectedItem(in: .apps), "com.a")
        XCTAssertNil(history.selectedItem(in: .prompts))
        history.open(.pane(.privacy))
        XCTAssertEqual(history.selectedItem(in: .apps), "com.a")
    }

    func testSelectingAnItemTargetsThatItemRegardlessOfListOrder() {
        var history = SettingsHistory(current: .pane(.apps))

        history.selectItem("com.b", in: .apps)
        XCTAssertEqual(history.selectedItem(in: .apps), "com.b")

        history.selectItem("com.a", in: .apps)
        XCTAssertEqual(history.selectedItem(in: .apps), "com.a")
        XCTAssertNil(history.selectedItem(in: .prompts))

        history.selectItem("prompt-2", in: .prompts)
        XCTAssertEqual(history.selectedItem(in: .prompts), "prompt-2")
        XCTAssertEqual(history.selectedItem(in: .apps), "com.a")

        history.selectItem(nil, in: .apps)
        XCTAssertNil(history.selectedItem(in: .apps))
    }
}
