import XCTest

@testable import Aria

final class PairedPhoneNameTests: XCTestCase {
    func testAKnownModelReadsAsMacOSNamesIt() {
        XCTAssertEqual(Self.phone("iPhone11,2").name, "iPhone XS")
    }

    func testAnUnknownOrMissingModelReadsAsIPhone() {
        XCTAssertEqual(Self.phone("iPhone99,9").name, "iPhone")
        XCTAssertEqual(Self.phone(nil).name, "iPhone")
    }

    private static func phone(_ hardwareModel: String?) -> PairedPhone {
        PairedPhone(fingerprint: Data(count: 32), hardwareModel: hardwareModel, pairedAt: nil)
    }
}
