import XCTest

@testable import Aria

final class AriaKeyTests: XCTestCase {
    func testEveryKeyRoundTripsTheWire() {
        for key in AriaKey.allCases {
            XCTAssertEqual(AriaKey(wire: key.wire), key)
        }
    }

    func testOffIsAKeyToChooseAndNeverAFailure() {
        XCTAssertEqual(AriaKey.off.title, "Off")
        XCTAssertTrue(AriaKey.allCases.contains(.off))
        XCTAssertEqual(AriaKey.off.gesture(for: .shift), "Off")
        XCTAssertTrue(AriaKey.isReady(key: .off, isServing: false))
        XCTAssertFalse(AriaKey.isReady(key: .rightOption, isServing: false))
        XCTAssertFalse(AriaKey.isReady(key: nil, isServing: false))
        XCTAssertTrue(AriaKey.isReady(key: .fn, isServing: true))
    }

    func testUnspecifiedWireIsNil() {
        XCTAssertNil(AriaKey(wire: .unspecified))
        XCTAssertNil(AriaKey(wire: .UNRECOGNIZED(99)))
    }
}
