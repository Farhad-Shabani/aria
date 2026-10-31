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
    var invalid: InvalidSettings?

    func invalidSettings() async -> InvalidSettings? { invalid }

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

    func testPollOnceReflectsRunningDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .running)
    }

    func testPollOnceReflectsStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .stopped)
    }

    func testToggleFromStoppedStartsDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertNil(poller.failure)
    }

    func testToggleWaitsForTheSocketBeforeRunning() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        fake.startLag = 2
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertEqual(fake.startLag, 0)
    }

    func testToggleFromRunningStopsDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
    }

    func testToggleStartFailureSurfacesErrorAndRepolls() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.ariaBinaryMissing("/nope/bin/aria")
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
        XCTAssertEqual(poller.failure, .start)
    }

    func testAStartStoppedByAnInvalidFileNamesItsLineAndNotTheGenericFailure() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.commandFailed("boom")
        fake.invalid = InvalidSettings(line: 3)
        let poller = self.poller(fake)
        await poller.pollOnce()

        await poller.toggle()

        XCTAssertEqual(poller.failure, .settingsInvalid(InvalidSettings(line: 3)))
        XCTAssertEqual(
            poller.failure?.message,
            "Aria couldn’t start because your settings file has a mistake on line 3. Fix it, then turn Aria on.")
    }

    func testADaemonThatNeverListensWithAnInvalidFileNamesTheFile() async {
        let fake = FakeDaemonControl()
        fake.startLag = 100
        fake.invalid = InvalidSettings(line: nil)
        let poller = self.poller(fake)
        await poller.pollOnce()

        await poller.toggle()

        XCTAssertEqual(poller.failure, .settingsInvalid(InvalidSettings(line: nil)))
    }

    func testLoginItemsKeepsItsOwnMessageOverAnInvalidFile() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.agentDisabled
        fake.invalid = InvalidSettings(line: 3)
        let poller = self.poller(fake)
        await poller.pollOnce()

        await poller.toggle()

        XCTAssertEqual(poller.failure, .backgroundItemOff)
    }

    func testToggleNamesTheFailedActionWithoutItsDetail() async {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.stopError = DaemonClientError.commandFailed("launchctl kill exited with 3: No such process")
        let poller = self.poller(fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.failure, .stop)
        XCTAssertFalse(poller.failure?.message.contains("launchctl") ?? true)
    }

    func testTogglePointsAtLoginItemsWhenTheAgentIsOff() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.agentDisabled
        let poller = self.poller(fake)
        await poller.toggle()
        XCTAssertEqual(poller.failure, .backgroundItemOff)
    }

    func testToggleReportsADaemonThatNeverListens() async {
        let fake = FakeDaemonControl()
        fake.startLag = 100
        let poller = self.poller(fake)
        await poller.toggle()
        XCTAssertEqual(poller.failure, .unresponsive)
    }

    func testToggleClearsPreviousError() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.commandFailed("boom")
        let poller = self.poller(fake)
        await poller.toggle()
        XCTAssertNotNil(poller.failure)
        fake.startError = nil
        await poller.toggle()
        XCTAssertNil(poller.failure)
        XCTAssertEqual(poller.state, .running)
    }

    func testEnsureRunningStartsAStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertTrue(fake.running)
        XCTAssertEqual(poller.state, .running)
    }

    func testEnsureRunningLeavesARunningDaemonAlone() async {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.startError = DaemonClientError.commandFailed("must not start")
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertNil(poller.failure)
        XCTAssertEqual(poller.state, .running)
    }

    func testEnsureRunningStartsNothingAfterAFailedRetirement() async {
        let fake = FakeDaemonControl()
        fake.retireError = DaemonClientError.commandFailed("uninstall failed")
        let poller = self.poller(fake)
        await poller.ensureRunning()
        XCTAssertEqual(poller.failure, .start)
        XCTAssertEqual(fake.calls, [])
    }

    func testRestartStopsThenStartsARunningDaemon() async throws {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = self.poller(fake)
        await poller.pollOnce()
        try await poller.restart()
        XCTAssertEqual(fake.calls, ["stop", "start"])
        XCTAssertEqual(poller.state, .running)
    }

    func testRestartOfAStoppedDaemonOnlyStarts() async throws {
        let fake = FakeDaemonControl()
        let poller = self.poller(fake)
        await poller.pollOnce()
        try await poller.restart()
        XCTAssertEqual(fake.calls, ["start"])
        XCTAssertEqual(poller.state, .running)
    }

    /// Revoke All: the pairing file goes only once the process exits.
    func testRestartRunsWhileStoppedOnlyOnceTheDaemonHasExited() async throws {
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

    func testRestartOfADaemonThatWillNotExitSkipsWhileStoppedAndThrows() async {
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
        XCTAssertEqual(poller.failure, .stillRunning)
    }

    func testRestartStartsAgainEvenWhenWhileStoppedFails() async {
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
