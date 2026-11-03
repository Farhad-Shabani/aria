import XCTest

@testable import Aria

final class AriaKeyTests: XCTestCase {
    func testEveryKeyRoundTripsTheWire() {
        for key in AriaKey.allCases {
            XCTAssertEqual(AriaKey(wire: key.wire), key)
        }
    }

    func testUnspecifiedWireIsNil() {
        XCTAssertNil(AriaKey(wire: .unspecified))
        XCTAssertNil(AriaKey(wire: .UNRECOGNIZED(99)))
    }
}
