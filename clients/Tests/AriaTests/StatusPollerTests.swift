import XCTest

@testable import Aria

final class FakeDaemonControl: DaemonControlling, @unchecked Sendable {
    var running = false
    var startError: Error?
    var stopError: Error?
    /// Probes that still see the daemon down after `start()` returns.
    var startLag = 0

    func isRunning() async -> Bool {
        if startLag > 0 {
            startLag -= 1
            return false
        }
        return running
    }

    private(set) var calls: [String] = []

    func start() async throws {
        calls.append("start")
        if let error = startError { throw error }
        running = true
    }

    func stop() async throws {
        calls.append("stop")
        if let error = stopError { throw error }
        running = false
    }
}

@MainActor
final class StatusPollerTests: XCTestCase {
    func test_pollOnce_reflectsRunningDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .running)
    }

    func test_pollOnce_reflectsStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        XCTAssertEqual(poller.state, .stopped)
    }

    func test_toggle_fromStopped_startsDaemon() async {
        let fake = FakeDaemonControl()
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertNil(poller.lastError)
    }

    func test_toggle_waitsForTheSocketBeforeRunning() async {
        let fake = FakeDaemonControl()
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        fake.startLag = 2
        await poller.toggle()
        XCTAssertEqual(poller.state, .running)
        XCTAssertEqual(fake.startLag, 0)
    }

    func test_toggle_fromRunning_stopsDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
    }

    func test_toggle_startFailure_surfacesErrorAndRepolls() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.ariaBinaryMissing("/nope/bin/aria")
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        await poller.toggle()
        XCTAssertEqual(poller.state, .stopped)
        XCTAssertEqual(poller.lastError, "aria binary not found at /nope/bin/aria")
    }

    func test_toggle_clearsPreviousError() async {
        let fake = FakeDaemonControl()
        fake.startError = DaemonClientError.commandFailed("boom")
        let poller = StatusPoller(client: fake)
        await poller.toggle()
        XCTAssertNotNil(poller.lastError)
        fake.startError = nil
        await poller.toggle()
        XCTAssertNil(poller.lastError)
        XCTAssertEqual(poller.state, .running)
    }

    func test_ensureRunning_startsAStoppedDaemon() async {
        let fake = FakeDaemonControl()
        let poller = StatusPoller(client: fake)
        await poller.ensureRunning()
        XCTAssertTrue(fake.running)
        XCTAssertEqual(poller.state, .running)
    }

    func test_ensureRunning_leavesARunningDaemonAlone() async {
        let fake = FakeDaemonControl()
        fake.running = true
        fake.startError = DaemonClientError.commandFailed("must not start")
        let poller = StatusPoller(client: fake)
        await poller.ensureRunning()
        XCTAssertNil(poller.lastError)
        XCTAssertEqual(poller.state, .running)
    }

    func test_restart_stopsThenStartsARunningDaemon() async {
        let fake = FakeDaemonControl()
        fake.running = true
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        await poller.restart()
        XCTAssertEqual(fake.calls, ["stop", "start"])
        XCTAssertEqual(poller.state, .running)
    }

    func test_restart_ofAStoppedDaemon_onlyStarts() async {
        let fake = FakeDaemonControl()
        let poller = StatusPoller(client: fake)
        await poller.pollOnce()
        await poller.restart()
        XCTAssertEqual(fake.calls, ["start"])
        XCTAssertEqual(poller.state, .running)
    }
}
