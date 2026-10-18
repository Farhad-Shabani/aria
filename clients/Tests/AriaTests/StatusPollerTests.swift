import XCTest

@testable import Aria

final class FakeDaemonControl: DaemonControlling, @unchecked Sendable {
    var running = false
    var startError: Error?
    var stopError: Error?
    var retireError: Error?
    /// Probes that still see the daemon down after `start()` returns.
    var startLag = 0
    /// Probes that still see the process alive after `stop()` returns.
    var exitLag = 0
    var ignoresStop = false

    func retireLegacyService() async throws {
        if let error = retireError { throw error }
    }

    func isRunning() async -> Bool {
        if startLag > 0 {
            startLag -= 1
            return false
        }
        return running
    }

    func hasExited() async -> Bool {
        if exitLag > 0 {
            exitLag -= 1
            return false
        }
        return !running
    }

    private(set) var calls: [String] = []

    func record(_ call: String) {
        calls.append(call)
    }

    func start() async throws {
        calls.append("start")
        if let error = startError { throw error }
        running = true
    }

    func stop() async throws {
        calls.append("stop")
        if let error = stopError { throw error }
        if !ignoresStop { running = false }
    }
}

@MainActor
final class StatusPollerTests: XCTestCase {
    private func poller(_ fake: FakeDaemonControl) -> StatusPoller {
        StatusPoller(client: fake, probeInterval: .milliseconds(1))
    }

    func test_pollOnce_reflectsRunningDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .running)
    }

    func test_pollOnce_reflectsStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .stopped)
    }

    func test_toggle_fromStopped_startsDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertNil(poller.lastError)
    }

    func test_toggle_waitsForTheSocketBeforeRunning() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        fake.startLag = 2
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertEqual(fake.startLag, 0)
    }

    func test_toggle_fromRunning_stopsDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
    }

    func test_toggle_startFailure_surfacesErrorAndRepolls() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.ariaBinaryMissing("/nope/bin/aria")
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
        XCTAssertEqual(poller.lastError, "aria binary not found at /nope/bin/aria")
    }

    func test_toggle_clearsPreviousError() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.commandFailed("boom")
        let poller = self.poller(fake)
        await poller.toggle()
        XCTAssertNotNil(poller.lastError)
        fake.startError = nil
        await poller.toggle()
        XCTAssertNil(poller.lastError)
        XCTAssertEqual(poller.state, .running)
    }

    func test_ensureRunning_startsAStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertTrue(fake.running)
        XCTAssertEqual(poller.state, .running)
    }

    func test_ensureRunning_leavesARunningDaemonAlone() async {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.startError = DaemonClientError.commandFailed("must not start")
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertNil(poller.lastError)
        XCTAssertEqual(poller.state, .running)
    }

    func test_ensureRunning_startsNothingAfterAFailedRetirement() async {
        let fake = FakeDaemonControl()
        fake.retireError = DaemonClientError.commandFailed("uninstall failed")
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertEqual(poller.lastError, "uninstall failed")
        XCTAssertEqual(fake.calls, [])
    }

    func test_restart_stopsThenStartsARunningDaemon() async throws {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        try await poller.restart()
        XCTAssertEqual(fake.calls, ["stop", "start"])
        XCTAssertEqual(poller.state, .running)
    }

    func test_restart_ofAStoppedDaemon_onlyStarts() async throws {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        try await poller.restart()
        XCTAssertEqual(fake.calls, ["start"])
        XCTAssertEqual(poller.state, .running)
    }

    /// Revoke All: the pairing file goes only once the process exits.
    func test_restart_runsWhileStoppedOnlyOnceTheDaemonHasExited() async throws {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.exitLag = 2
        let poller = self.poller(fake)
        await poller.pollOnce()
        try await poller.restart(whileStopped: {
            XCTAssertEqual(fake.exitLag, 0, "the daemon had not exited")
            fake.record("whileStopped")
        })
        XCTAssertEqual(fake.calls, ["stop", "whileStopped", "start"])
    }

    func test_restart_ofADaemonThatWillNotExit_skipsWhileStoppedAndThrows() async {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.ignoresStop = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        do {
            try await poller.restart(whileStopped: { fake.record("whileStopped") })
            XCTFail("a daemon that never exits must not be restarted around")
        } catch StatusPoller.RestartFailure.stillRunning {
        } catch {
            XCTFail("unexpected \(error)")
        }
        XCTAssertEqual(fake.calls, ["stop"])
        XCTAssertEqual(poller.state, .running)
    }

    func test_restart_startsAgainEvenWhenWhileStoppedFails() async {
        struct Failed: Error {}
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        do {
            try await poller.restart(whileStopped: { throw Failed() })
            XCTFail("the failure must surface")
        } catch is Failed {
        } catch {
            XCTFail("unexpected \(error)")
        }
        XCTAssertEqual(fake.calls, ["stop", "start"])
        XCTAssertEqual(poller.state, .running)
    }
}
