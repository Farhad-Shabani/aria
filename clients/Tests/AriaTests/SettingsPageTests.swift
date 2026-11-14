import XCTest

@testable import Aria

final class SettingsPageTests: XCTestCase {
    func testAPaneShowsItsOwnRows() {
        XCTAssertTrue(SettingsPage.pane(.general).shows(.speakingRate))
    }

    func testAnotherPaneDoesNotShowTheRow() {
        XCTAssertFalse(SettingsPage.pane(.privacy).shows(.speechModel))
    }

    /// Storage sits under General in the sidebar but shows none of General's rows.
    func testAPageUnderThePaneDoesNotShowItsRows() {
        XCTAssertFalse(SettingsPage.storage.shows(.speakingRate))
    }
}
