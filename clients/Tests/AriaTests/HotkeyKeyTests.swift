import XCTest

@testable import Aria

final class HotkeyKeyTests: XCTestCase {
    func testEveryKeyRoundTripsTheWire() {
        for key in HotkeyKey.allCases {
            XCTAssertEqual(HotkeyKey(wire: key.wire), key)
        }
    }

    func testUnspecifiedWireIsNil() {
        XCTAssertNil(HotkeyKey(wire: .unspecified))
        XCTAssertNil(HotkeyKey(wire: .UNRECOGNIZED(99)))
    }
}
