import XCTest

@testable import Aria

@MainActor
final class ListenStatusTests: XCTestCase {
    private struct DaemonFailure: Error {}

    private func event(_ stage: Aria_Daemon_V1_ListenStage) -> Aria_Daemon_V1_ListenEvent {
        var event = Aria_Daemon_V1_ListenEvent()
        event.stage = stage
        return event
    }

    func testAStartedSessionIsRunningUntilItEnds() {
        for ending in [Aria_Daemon_V1_ListenStage.stopped, .failed] {
            let status = ListenStatus(readStatus: { false })
            status.handle(event(.started))
            XCTAssertTrue(status.isRunning)
            status.handle(event(ending))
            XCTAssertFalse(status.isRunning)
        }
    }

    func testAStageDuringAReadWinsOverTheRead() async {
        let reading = AsyncStream<Void>.makeStream()
        let finishing = AsyncStream<Void>.makeStream()
        let status = ListenStatus(readStatus: {
            reading.continuation.yield()
            for await _ in finishing.stream { break }
            return false
        })

        async let caughtUp: Void = status.catchUp()
        for await _ in reading.stream { break }
        status.handle(event(.started))
        finishing.continuation.yield()
        await caughtUp

        XCTAssertTrue(status.isRunning)
    }

    func testAFailedReadLeavesTheStateAlone() async {
        let status = ListenStatus(readStatus: { throw DaemonFailure() })
        status.handle(event(.started))
        await status.catchUp()
        XCTAssertTrue(status.isRunning)
    }

    func testStoppingEndsTheSession() async {
        let status = ListenStatus(readStatus: { true }, stopSession: {})
        status.handle(event(.started))
        await status.stop()
        XCTAssertFalse(status.isRunning)
        XCTAssertFalse(status.didFailToStop)
        XCTAssertFalse(status.isStopping)
    }

    func testAStopErrorFromASessionThatEndedIsNotAFailedStop() async {
        let status = ListenStatus(readStatus: { false }, stopSession: { throw DaemonFailure() })
        status.handle(event(.started))
        await status.stop()
        XCTAssertFalse(status.isRunning)
        XCTAssertFalse(status.didFailToStop)
    }

    func testASessionThatOutlivesAStopErrorIsAFailedStop() async {
        let status = ListenStatus(readStatus: { true }, stopSession: { throw DaemonFailure() })
        status.handle(event(.started))
        await status.stop()
        XCTAssertTrue(status.isRunning)
        XCTAssertTrue(status.didFailToStop)
    }

    func testANewSessionClearsAnEarlierFailedStop() async {
        let status = ListenStatus(readStatus: { true }, stopSession: { throw DaemonFailure() })
        status.handle(event(.started))
        await status.stop()
        status.handle(event(.stopped))
        status.handle(event(.started))
        XCTAssertFalse(status.didFailToStop)
    }

    func testReconcilingClearsASessionThatEndedWithoutAStage() async {
        let daemon = ScriptedDaemon(reads: [true, false])
        let status = reconciled(by: daemon)
        status.startReconciling()
        await settled { daemon.readCount >= 2 && !status.isRunning }
        XCTAssertFalse(status.isRunning)
        status.handle(nil)
    }

    func testALostStreamStopsReconciling() async {
        let daemon = ScriptedDaemon(reads: [true])
        let status = reconciled(by: daemon)
        status.startReconciling()
        await settled { status.isRunning }
        status.handle(nil)
        let readsWhenLost = daemon.readCount

        try? await Task.sleep(for: .milliseconds(50))

        XCTAssertFalse(status.isRunning)
        XCTAssertLessThanOrEqual(daemon.readCount, readsWhenLost + 1)
    }

    private func reconciled(by daemon: ScriptedDaemon) -> ListenStatus {
        ListenStatus(readStatus: { daemon.read() }, reconcileInterval: .milliseconds(1))
    }

    private func settled(_ condition: () -> Bool) async {
        for _ in 0..<500 where !condition() {
            try? await Task.sleep(for: .milliseconds(2))
        }
    }
}

/// Answers each read from a script, repeating the last answer once it runs out.
private final class ScriptedDaemon: @unchecked Sendable {
    // Every access holds `lock`.
    private let lock = NSLock()
    private var reads: [Bool]
    private var reachedCount = 0

    init(reads: [Bool]) {
        self.reads = reads
    }

    var readCount: Int {
        lock.withLock { reachedCount }
    }

    func read() -> Bool {
        lock.withLock {
            reachedCount += 1
            return reads.count > 1 ? reads.removeFirst() : reads[0]
        }
    }
}
