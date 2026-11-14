import XCTest

@testable import Aria

final class SettingsRowTests: XCTestCase {
    func testEveryRowOpensASidebarPane() {
        let sidebar = Set(SettingsPane.sidebarGroups.flatMap { $0 })
        for row in SettingsRow.allCases {
            XCTAssertTrue(sidebar.contains(row.pane), "\(row) opens \(row.pane)")
        }
    }
}
