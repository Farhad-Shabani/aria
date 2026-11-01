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

    private func status() -> ListenStatus {
        ListenStatus(readStatus: { false })
    }

    func testAStartedSessionIsRunningUntilItEnds() {
        for ending in [Aria_Daemon_V1_ListenStage.stopped, .failed] {
            let status = status()
            status.handle(event(.started))
            XCTAssertTrue(status.isRunning)
            status.handle(event(ending))
            XCTAssertFalse(status.isRunning)
        }
    }

    func testASegmentShowsASessionWhoseStartWasMissed() {
        let status = status()
        status.handle(event(.segmentTranscribed))
        XCTAssertTrue(status.isRunning)
    }

    func testALostStreamClearsTheSession() {
        let status = status()
        status.handle(event(.started))
        status.handle(nil)
        XCTAssertFalse(status.isRunning)
    }

    func testAnUnknownStageChangesNothing() {
        let status = status()
        status.handle(event(.started))
        status.handle(event(.UNRECOGNIZED(99)))
        XCTAssertTrue(status.isRunning)
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

    func testReconcilingRecoversAfterAFailedRead() async {
        let daemon = ScriptedDaemon(reads: [.failure(DaemonFailure()), .success(true)])
        let status = reconciled(by: daemon)
        status.startReconciling()
        await settled { status.isRunning }
        XCTAssertTrue(status.isRunning)
        status.handle(nil)
    }

    func testReconcilingClearsASessionThatEndedWithoutAStage() async {
        let daemon = ScriptedDaemon(reads: [.success(true), .success(false)])
        let status = reconciled(by: daemon)
        status.startReconciling()
        await settled { daemon.readCount >= 2 && !status.isRunning }
        XCTAssertFalse(status.isRunning)
        status.handle(nil)
    }

    func testALostStreamStopsReconciling() async {
        let daemon = ScriptedDaemon(reads: [.success(true)])
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
        ListenStatus(readStatus: { try daemon.read() }, reconcileInterval: .milliseconds(1))
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
    private var reads: [Result<Bool, any Error>]
    private var reachedCount = 0

    init(reads: [Result<Bool, any Error>]) {
        self.reads = reads
    }

    var readCount: Int {
        lock.withLock { reachedCount }
    }

    func read() throws -> Bool {
        try lock.withLock {
            reachedCount += 1
            let answer = reads.count > 1 ? reads.removeFirst() : reads[0]
            return try answer.get()
        }
    }
}
