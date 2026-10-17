import XCTest

@testable import Aria

final class PairingInviteTests: XCTestCase {
    private let fingerprint = PairingInvite.fingerprint(ofDER: Data([0x30, 0x82, 0x01, 0xff, 0xfe, 0x00, 0x7f]))

    private func inviteText(instance: String = "Aria", code: String = "c0de") throws -> String {
        let invite = PairingInvite(instanceName: instance, code: code, fingerprint: fingerprint)
        return try XCTUnwrap(invite.url?.absoluteString)
    }

    func test_url_roundTrips() throws {
        let invite = PairingInvite(
            instanceName: "Aria (Alex's MacBook)", code: "c0de", fingerprint: fingerprint)
        let url = try XCTUnwrap(invite.url)
        XCTAssertEqual(url.scheme, "aria-pair")
        XCTAssertEqual(url.host, "v1")
        XCTAssertEqual(PairingInvite(url: url), invite)
    }

    func test_wrongSchemeOrVersion_isRejected() throws {
        let good = try inviteText()
        XCTAssertNil(PairingInvite(url: URL(string: good.replacingOccurrences(of: "aria-pair://", with: "https://"))!))
        XCTAssertNil(PairingInvite(url: URL(string: good.replacingOccurrences(of: "//v1?", with: "//v2?"))!))
    }

    func test_missingInstanceOrCode_isRejected() throws {
        let good = try inviteText()
        XCTAssertNil(
            PairingInvite(url: URL(string: good.replacingOccurrences(of: "instance=Aria", with: "instance="))!))
        XCTAssertNil(PairingInvite(url: URL(string: good.replacingOccurrences(of: "code=c0de", with: "code="))!))
        XCTAssertNil(PairingInvite(url: URL(string: good.replacingOccurrences(of: "code=c0de&", with: ""))!))
    }

    func test_missingMalformedOrShortFingerprint_isRejected() {
        XCTAssertNil(PairingInvite(url: URL(string: "aria-pair://v1?instance=Aria&code=ab")!))
        XCTAssertNil(PairingInvite(url: URL(string: "aria-pair://v1?instance=Aria&code=ab&fp=")!))
        XCTAssertNil(PairingInvite(url: URL(string: "aria-pair://v1?instance=Aria&code=ab&fp=%%%")!))
        XCTAssertNil(PairingInvite(url: URL(string: "aria-pair://v1?instance=Aria&code=ab&fp=MDA")!))
    }

    func test_fingerprint_isSHA256() {
        // FIPS 180-4 vector for "abc".
        let expected = Data(
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
                0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
            ]
        )
        XCTAssertEqual(PairingInvite.fingerprint(ofDER: Data("abc".utf8)), expected)
    }
}
