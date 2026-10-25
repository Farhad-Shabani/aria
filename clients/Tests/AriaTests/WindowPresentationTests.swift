import XCTest

@testable import Aria

@MainActor
final class WindowPresentationTests: XCTestCase {
    func testLaunchWaitsForSettingsAction() {
        let presentation = WindowPresentation()
        var opened = 0

        presentation.didLaunch(atLogin: false)
        presentation.register { opened += 1 }
        presentation.register { opened += 1 }

        XCTAssertEqual(opened, 1)
    }

    func testLaunchAndReopenPresentSettings() {
        let presentation = WindowPresentation()
        var opened = 0
        presentation.register { opened += 1 }

        presentation.didLaunch(atLogin: false)
        presentation.request()

        XCTAssertEqual(opened, 2)
    }

    func testLoginStaysQuietUntilReopened() {
        let presentation = WindowPresentation()
        var opened = 0

        presentation.didLaunch(atLogin: true)
        presentation.register { opened += 1 }
        XCTAssertEqual(opened, 0)

        presentation.request()
        XCTAssertEqual(opened, 1)
    }
}
