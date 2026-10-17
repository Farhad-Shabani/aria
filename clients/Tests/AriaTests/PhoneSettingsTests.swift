import GRPCCore
import XCTest

@testable import Aria

final class PhoneSettingsTests: XCTestCase {
    func test_status_tellsRelayOffFromDaemonOffFromTheRest() {
        XCTAssertEqual(PhoneSettings.status(for: RPCError(code: .unimplemented, message: "")), .relayOff)
        XCTAssertEqual(PhoneSettings.status(for: RPCError(code: .unavailable, message: "")), .daemonOff)
        struct Boom: LocalizedError {
            var errorDescription: String? { "boom" }
        }
        XCTAssertEqual(PhoneSettings.status(for: Boom()), .failed("boom"))
    }

    func test_nextPoll_waitsForExpiryWhenSooner_andNeverSpins() {
        let now = Date(timeIntervalSinceReferenceDate: 1_000)
        XCTAssertEqual(PhoneSettings.nextPoll(expiresAt: now.addingTimeInterval(30), now: now), .seconds(2))
        XCTAssertEqual(PhoneSettings.nextPoll(expiresAt: now.addingTimeInterval(0.5), now: now), .milliseconds(500))
        XCTAssertEqual(PhoneSettings.nextPoll(expiresAt: now, now: now), .milliseconds(250))
    }

    func test_wasRedeemed_onlyWhenTheCodeHadTimeLeft() {
        let expiresAt = Date(timeIntervalSinceReferenceDate: 1_000)
        XCTAssertTrue(PhoneSettings.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt.addingTimeInterval(-30)))
        XCTAssertFalse(PhoneSettings.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt.addingTimeInterval(-0.5)))
        XCTAssertFalse(PhoneSettings.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt))
    }
}
