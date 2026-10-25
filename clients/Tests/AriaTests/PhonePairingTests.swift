import GRPCCore
import XCTest
import os

@testable import Aria

final class PhonePairingTests: XCTestCase {
    func test_status_tellsRelayOffFromDaemonOffFromTheRest() {
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .unimplemented, message: "")), .relayOff)
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .unavailable, message: "")), .daemonOff)
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .internalError, message: "raw detail")), .failed)
    }

    func test_nextPoll_waitsForExpiryWhenSooner_andNeverSpins() {
        let now = Date(timeIntervalSinceReferenceDate: 1_000)
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now.addingTimeInterval(30), now: now), .seconds(2))
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now.addingTimeInterval(0.5), now: now), .milliseconds(500))
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now, now: now), .milliseconds(250))
    }

    func test_wasRedeemed_onlyWhenTheCodeHadTimeLeft() {
        let expiresAt = Date(timeIntervalSinceReferenceDate: 1_000)
        XCTAssertTrue(PhonePairing.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt.addingTimeInterval(-30)))
        XCTAssertFalse(PhonePairing.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt.addingTimeInterval(-0.5)))
        XCTAssertFalse(PhonePairing.wasRedeemed(expiresAt: expiresAt, replacedAt: expiresAt))
    }

    @MainActor
    func test_poll_countsACodeReplacedEarlyAsAPairing() async {
        let now = Date.now
        let pairing = Self.pairing(invites: [
            Self.issued("a", expiresAt: now + 60), Self.issued("b", expiresAt: now + 120),
        ])

        await pairing.poll()
        XCTAssertNil(pairing.phonePairedAt)
        await pairing.poll()

        XCTAssertNotNil(pairing.phonePairedAt)
        XCTAssertEqual(pairing.status, .live(Self.invite("b"), expiresAt: now + 120))
    }

    @MainActor
    func test_poll_doesNotCountAnExpiredCodeAsAPairing() async {
        let now = Date.now
        let pairing = Self.pairing(invites: [
            Self.issued("a", expiresAt: now + 0.5), Self.issued("b", expiresAt: now + 60),
        ])

        await pairing.poll()
        await pairing.poll()

        XCTAssertNil(pairing.phonePairedAt)
    }

    @MainActor
    func test_turnOn_reportsAFailedSaveAndReleasesTheSwitch() async {
        struct Unwritable: LocalizedError {
            var errorDescription: String? { "read-only" }
        }
        let pairing = Self.pairing(invites: [], setPairingError: Unwritable())

        await pairing.turn(on: true)

        XCTAssertEqual(pairing.actionError, "Phone pairing could not be turned on.")
        XCTAssertFalse(pairing.isOn)
        XCTAssertEqual(pairing.status, .daemonOff)
    }

    @MainActor
    func test_revoke_removesThePairingFileWhileAriaIsStopped() async {
        let control = FakeDaemonControl()
        control.running = true
        let pairing = Self.pairing(invites: [], control: control) { control.record("remove") }

        await pairing.revoke()

        XCTAssertEqual(control.calls, ["stop", "remove", "start"])
        XCTAssertNil(pairing.actionError)
    }

    private static func invite(_ code: String) -> PairingInvite {
        PairingInvite(instanceName: "mac", code: code, fingerprint: Data())
    }

    private static func issued(_ code: String, expiresAt: Date) -> (invite: PairingInvite, expiresAt: Date) {
        (invite(code), expiresAt)
    }

    /// Fetches past the scripted invites find the daemon not listening.
    @MainActor
    private static func pairing(
        invites: [(invite: PairingInvite, expiresAt: Date)],
        setPairingError: (any Error)? = nil,
        control: FakeDaemonControl = FakeDaemonControl(),
        removePairingFile: @escaping @Sendable () throws -> Void = {}
    ) -> PhonePairing {
        let queue = OSAllocatedUnfairLock(initialState: invites)
        let daemon = PairingDaemon(
            issueInvite: {
                guard let next = queue.withLock({ $0.isEmpty ? nil : $0.removeFirst() }) else {
                    throw RPCError(code: .unavailable, message: "")
                }
                return next
            },
            setPairing: { _ in
                if let setPairingError { throw setPairingError }
            },
            removePairingFile: removePairingFile
        )
        return PhonePairing(poller: StatusPoller(client: control, probeInterval: .milliseconds(1)), daemon: daemon)
    }
}
