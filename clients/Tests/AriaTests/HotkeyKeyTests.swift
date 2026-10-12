import XCTest

@testable import Aria

final class HotkeyKeyTests: XCTestCase {
    func test_everyKey_roundTripsTheWire() {
        for key in HotkeyKey.allCases {
            XCTAssertEqual(HotkeyKey(wire: key.wire), key)
        }
    }

    func test_unspecifiedWire_isNil() {
        XCTAssertNil(HotkeyKey(wire: .unspecified))
        XCTAssertNil(HotkeyKey(wire: .UNRECOGNIZED(99)))
    }
}
