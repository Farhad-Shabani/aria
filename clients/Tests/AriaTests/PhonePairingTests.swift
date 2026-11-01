import GRPCCore
import XCTest
import os

@testable import Aria

final class PhonePairingTests: XCTestCase {
    func testStatusTellsRelayOffFromDaemonOffFromTheRest() {
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .unimplemented, message: "")), .relayOff)
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .unavailable, message: "")), .daemonOff)
        XCTAssertEqual(PhonePairing.status(for: RPCError(code: .internalError, message: "raw detail")), .failed)
    }

    func testNextPollWaitsForExpiryWhenSoonerAndNeverSpins() {
        let now = Date(timeIntervalSinceReferenceDate: 1_000)
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now.addingTimeInterval(30), now: now), .seconds(2))
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now.addingTimeInterval(0.5), now: now), .milliseconds(500))
        XCTAssertEqual(PhonePairing.nextPoll(expiresAt: now, now: now), .milliseconds(250))
    }

    @MainActor
    func testPollCountsARisingPairingCountAsAPairing() async {
        let now = Date.now
        let pairing = Self.pairing(invites: [
            Self.issued("a", expiresAt: now + 60), Self.issued("b", expiresAt: now + 120, pairingCount: 1),
        ])

        await pairing.poll()
        XCTAssertNil(pairing.phonePairedAt)
        await pairing.poll()

        XCTAssertNotNil(pairing.phonePairedAt)
        XCTAssertEqual(pairing.status, .live(Self.invite("b"), expiresAt: now + 120))
    }

    @MainActor
    func testPollDoesNotCountAReplacedCodeAloneAsAPairing() async {
        let now = Date.now
        let pairing = Self.pairing(invites: [
            Self.issued("a", expiresAt: now + 0.5), Self.issued("b", expiresAt: now + 60),
        ])

        await pairing.poll()
        await pairing.poll()

        XCTAssertNil(pairing.phonePairedAt)
        XCTAssertEqual(pairing.status, .live(Self.invite("b"), expiresAt: now + 60))
    }

    @MainActor
    func testARestartedDaemonDoesNotCountAsAPairing() async {
        let now = Date.now
        let pairing = Self.pairing(invites: [
            Self.issued("a", expiresAt: now + 60, pairingCount: 3), Self.issued("b", expiresAt: now + 60),
        ])

        await pairing.poll()
        await pairing.poll()

        XCTAssertNil(pairing.phonePairedAt)
    }

    @MainActor
    func testAPollAnsweredBeforeARestartIsDroppedAfterIt() async {
        let now = Date.now
        let gate = Gate()
        let calls = OSAllocatedUnfairLock(initialState: 0)
        let daemon = PairingDaemon(
            issueInvite: {
                let call = calls.withLock { calls in
                    calls += 1
                    return calls
                }
                guard call == 1 else { return Self.issued("after", expiresAt: now + 60) }
                await gate.wait()
                return Self.issued("before", expiresAt: now + 60)
            },
            setPairing: { _ in },
            removePairingFile: {}
        )
        let control = FakeDaemonControl()
        control.running = true
        let pairing = PhonePairing(
            poller: StatusPoller(client: control, probeInterval: .milliseconds(1)), daemon: daemon)
        let stale = Task { await pairing.poll() }
        await gate.reached()

        await pairing.turn(on: true)
        XCTAssertEqual(pairing.status, .live(Self.invite("after"), expiresAt: now + 60))
        gate.open()
        await stale.value

        XCTAssertEqual(pairing.status, .live(Self.invite("after"), expiresAt: now + 60))
        XCTAssertNil(pairing.phonePairedAt)
    }

    @MainActor
    func testTurnOnReportsAFailedSaveAndReleasesTheSwitch() async {
        struct Unwritable: LocalizedError {
            var errorDescription: String? { "read-only" }
        }
        let pairing = Self.pairing(invites: [], setPairingError: Unwritable())

        await pairing.turn(on: true)

        XCTAssertEqual(pairing.actionError, "Aria couldn’t turn phone pairing on. Try again.")
        XCTAssertFalse(pairing.isOn)
        XCTAssertEqual(pairing.status, .daemonOff)
    }

    @MainActor
    func testRevokeRemovesThePairingFileWhileAriaIsStopped() async {
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

    private static func issued(_ code: String, expiresAt: Date, pairingCount: UInt64 = 0) -> IssuedInvite {
        IssuedInvite(invite: invite(code), expiresAt: expiresAt, pairingCount: pairingCount)
    }

    /// Fetches past the scripted invites find the daemon not listening.
    @MainActor
    private static func pairing(
        invites: [IssuedInvite],
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
