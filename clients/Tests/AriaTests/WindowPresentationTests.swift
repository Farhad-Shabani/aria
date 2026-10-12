import XCTest

@testable import Aria

@MainActor
final class WindowPresentationTests: XCTestCase {
    func test_launch_waits_for_settings_action() {
        let presentation = WindowPresentation()
        var opened = 0

        presentation.didLaunch(atLogin: false)
        presentation.register { opened += 1 }
        presentation.register { opened += 1 }

        XCTAssertEqual(opened, 1)
    }

    func test_launch_and_reopen_present_settings() {
        let presentation = WindowPresentation()
        var opened = 0
        presentation.register { opened += 1 }

        presentation.didLaunch(atLogin: false)
        presentation.request()

        XCTAssertEqual(opened, 2)
    }

    func test_login_stays_quiet_until_reopened() {
        let presentation = WindowPresentation()
        var opened = 0

        presentation.didLaunch(atLogin: true)
        presentation.register { opened += 1 }
        XCTAssertEqual(opened, 0)

        presentation.request()
        XCTAssertEqual(opened, 1)
    }
}
