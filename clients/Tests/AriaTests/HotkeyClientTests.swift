import XCTest

@testable import Aria

final class HotkeyClientTests: XCTestCase {
    func testAReportedStateNeedsNoHealthCheck() async {
        let state = await HotkeyClient.resolve(.awaitingListenAccess) {
            XCTFail("a reported state is enough")
            return true
        }

        XCTAssertEqual(state, .needsInputMonitoring)
    }

    func testAnOlderDaemonFallsBackToHealth() async {
        let armed = await HotkeyClient.resolve(nil) { true }
        let unarmed = await HotkeyClient.resolve(nil) { false }

        XCTAssertEqual(armed, .armed)
        XCTAssertEqual(unarmed, .needsAccessibility)
    }
}
