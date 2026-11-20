import XCTest

@testable import Aria

final class PhoneNamesTests: XCTestCase {
    func testAKnownIdentifierReadsAsItsModel() {
        XCTAssertEqual(PhoneNames.name(for: "iPhone18,1"), "iPhone 17 Pro")
        XCTAssertEqual(PhoneNames.name(for: "iPhone14,6"), "iPhone SE (3rd generation)")
    }

    func testAnUnknownOrMissingIdentifierReadsAsIPhone() {
        XCTAssertEqual(PhoneNames.name(for: "iPhone99,9"), "iPhone")
        XCTAssertEqual(PhoneNames.name(for: nil), "iPhone")
    }
}
