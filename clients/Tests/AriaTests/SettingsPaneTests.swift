import XCTest

@testable import Aria

final class SettingsPaneTests: XCTestCase {
    func testSidebarGroupsListEveryPaneOnceInOrder() {
        XCTAssertEqual(SettingsPane.sidebarGroups.flatMap { $0 }, SettingsPane.allCases)
    }
}
