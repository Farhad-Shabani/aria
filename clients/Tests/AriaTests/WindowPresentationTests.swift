import XCTest

@testable import Aria

@MainActor
final class WindowPresentationTests: XCTestCase {
    func testARequestWaitsForTheSceneAndOpensOnce() {
        let presentation = WindowPresentation()
        var opened = 0

        presentation.request()
        presentation.register { opened += 1 }
        presentation.register { opened += 1 }

        XCTAssertEqual(opened, 1)
    }

    func testARequestAfterTheSceneOpensAtOnce() {
        let presentation = WindowPresentation()
        var opened = 0
        presentation.register { opened += 1 }

        presentation.request()

        XCTAssertEqual(opened, 1)
    }
}
