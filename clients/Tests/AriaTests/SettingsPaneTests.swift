import XCTest

@testable import Aria

final class SettingsPaneTests: XCTestCase {
    func test_sidebarGroups_listEveryPaneOnceInOrder() {
        XCTAssertEqual(SettingsPane.sidebarGroups.flatMap { $0 }, SettingsPane.allCases)
    }
}
