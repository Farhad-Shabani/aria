import XCTest

@testable import Aria

final class SettingsPaneTests: XCTestCase {
    func testSidebarGroupsListEveryPaneButFinishSetupOnceInOrder() {
        XCTAssertEqual(
            SettingsPane.sidebarGroups.flatMap { $0 }, SettingsPane.allCases.filter { $0 != .finishSetup })
    }
}
