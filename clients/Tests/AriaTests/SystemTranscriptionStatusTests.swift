import Combine
import XCTest

@testable import Aria

@MainActor
final class SystemTranscriptionStatusTests: XCTestCase {
    private struct Unreachable: Error {}

    private func event(_ stage: Aria_Daemon_V1_SystemTranscriptionStage) -> Aria_Daemon_V1_SystemTranscriptionEvent {
        var event = Aria_Daemon_V1_SystemTranscriptionEvent()
        event.stage = stage
        return event
    }

    func testAStartedSessionIsRunningUntilItEnds() {
        for ending in [Aria_Daemon_V1_SystemTranscriptionStage.stopped, .failed] {
            let status = SystemTranscriptionStatus(readStatus: { .idle })
            status.handle(event(.started))
            XCTAssertTrue(status.isRunning)
            status.handle(event(ending))
            XCTAssertFalse(status.isRunning)
        }
    }

    func testAnEndedSessionFallsSilent() {
        for ending in [Aria_Daemon_V1_SystemTranscriptionStage.stopped, .failed] {
            let status = SystemTranscriptionStatus(readStatus: { .idle })
            status.handle(event(.started))
            status.hear(level: 0.8)
            XCTAssertEqual(status.level.value, 0.8)
            status.handle(event(ending))
            XCTAssertEqual(status.level.value, 0)
        }
    }

    func testALevelShowsASessionThatStartedBeforeAriaOpened() {
        let status = SystemTranscriptionStatus(readStatus: { .idle })
        status.hear(level: 0.4)
        XCTAssertTrue(status.isRunning)
        XCTAssertEqual(status.level.value, 0.4)
    }

    func testALevelDuringASessionPublishesNothing() {
        let status = SystemTranscriptionStatus(readStatus: { .running })
        status.handle(event(.started))
        var published = 0
        let watching = status.objectWillChange.sink { published += 1 }
        status.hear(level: 0.3)
        status.hear(level: 0.6)
        XCTAssertEqual(published, 0)
        XCTAssertEqual(status.level.value, 0.6)
        watching.cancel()
    }

    func testALevelLateForAStoppedSessionLeavesItStopped() async {
        let status = SystemTranscriptionStatus(readStatus: { .idle }, stopSession: {})
        status.handle(event(.started))
        await status.stop()
        status.hear(level: 0.5)
        XCTAssertFalse(status.isRunning)
        XCTAssertEqual(status.level.value, 0)
        status.handle(event(.started))
        status.hear(level: 0.5)
        XCTAssertEqual(status.level.value, 0.5)
    }

    func testALevelDuringAReadKeepsTheRead() async {
        let reading = AsyncStream<Void>.makeStream()
        let finishing = AsyncStream<Void>.makeStream()
        let status = SystemTranscriptionStatus(readStatus: {
            reading.continuation.yield()
            for await _ in finishing.stream { break }
            return .idle
        })
        status.handle(event(.started))

        async let caughtUp: Void = status.catchUp()
        for await _ in reading.stream { break }
        status.hear(level: 0.5)
        finishing.continuation.yield()
        await caughtUp

        XCTAssertFalse(status.isRunning)
    }

    func testAStageDuringAReadWinsOverTheRead() async {
        let reading = AsyncStream<Void>.makeStream()
        let finishing = AsyncStream<Void>.makeStream()
        let status = SystemTranscriptionStatus(readStatus: {
            reading.continuation.yield()
            for await _ in finishing.stream { break }
            return .idle
        })

        async let caughtUp: Void = status.catchUp()
        for await _ in reading.stream { break }
        status.handle(event(.started))
        finishing.continuation.yield()
        await caughtUp

        XCTAssertTrue(status.isRunning)
    }

    func testAFailedReadLeavesTheStateAlone() async {
        let status = SystemTranscriptionStatus(readStatus: { throw Unreachable() })
        status.handle(event(.started))
        await status.catchUp()
        XCTAssertTrue(status.isRunning)
    }

    func testStoppingEndsTheSession() async {
        let status = SystemTranscriptionStatus(readStatus: { .running }, stopSession: {})
        status.handle(event(.started))
        await status.stop()
        XCTAssertFalse(status.isRunning)
        XCTAssertFalse(status.didFailToStop)
        XCTAssertFalse(status.isStopping)
    }

    func testAStopErrorFromASessionThatEndedIsNotAFailedStop() async {
        let status = SystemTranscriptionStatus(readStatus: { .idle }, stopSession: { throw Unreachable() })
        status.handle(event(.started))
        await status.stop()
        XCTAssertFalse(status.isRunning)
        XCTAssertFalse(status.didFailToStop)
    }

    func testASessionThatOutlivesAStopErrorIsAFailedStop() async {
        let status = SystemTranscriptionStatus(readStatus: { .running }, stopSession: { throw Unreachable() })
        status.handle(event(.started))
        await status.stop()
        XCTAssertTrue(status.isRunning)
        XCTAssertTrue(status.didFailToStop)
    }

    func testANewSessionClearsAnEarlierFailedStop() async {
        let status = SystemTranscriptionStatus(readStatus: { .running }, stopSession: { throw Unreachable() })
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

    private func reconciled(by daemon: ScriptedDaemon) -> SystemTranscriptionStatus {
        SystemTranscriptionStatus(readStatus: { daemon.read() }, reconcileInterval: .milliseconds(1))
    }

    private func settled(_ condition: () -> Bool) async {
        for _ in 0..<500 where !condition() {
            try? await Task.sleep(for: .milliseconds(2))
        }
    }

    func testAnAcceptedStartRunsAndARefusedOneSaysSo() async {
        let started = SystemTranscriptionStatus(readStatus: { .idle }, startSession: {})
        await started.start()
        XCTAssertTrue(started.isRunning)
        XCTAssertFalse(started.didFailToStart)

        let refused = SystemTranscriptionStatus(readStatus: { .idle }, startSession: { throw Unreachable() })
        await refused.start()
        XCTAssertFalse(refused.isRunning)
        XCTAssertTrue(refused.didFailToStart)
        XCTAssertFalse(refused.isStarting)
    }

    func testAStartedSessionNamesItsFile() {
        let status = SystemTranscriptionStatus(readStatus: { .idle })
        var started = event(.started)
        started.transcriptPath = "/tmp/standup.txt"
        status.handle(started)
        XCTAssertEqual(status.transcriptFile, URL(filePath: "/tmp/standup.txt"))
    }

    func testAReadNamesTheFileOfASessionJoinedLate() async {
        let file = URL(filePath: "/tmp/standup.txt")
        let status = SystemTranscriptionStatus(readStatus: { .init(isRunning: true, transcriptFile: file) })
        await status.catchUp()
        XCTAssertEqual(status.transcriptFile, file)
    }

    func testAReadThatKnowsNoFileKeepsTheLastOne() async {
        let status = SystemTranscriptionStatus(readStatus: { .idle })
        var started = event(.started)
        started.transcriptPath = "/tmp/standup.txt"
        status.handle(started)
        status.handle(event(.stopped))
        await status.catchUp()
        XCTAssertEqual(status.transcriptFile, URL(filePath: "/tmp/standup.txt"))
    }

    func testASessionJoinedLateDatesItsStartFromTheFirstOffset() {
        let status = SystemTranscriptionStatus(readStatus: { .running })
        let now = Date(timeIntervalSince1970: 1000)
        var segment = event(.segmentTranscribed)
        segment.offset = .init(seconds: 90)

        status.handle(segment, now: now)
        status.handle(segment, now: now.addingTimeInterval(5))

        XCTAssertEqual(status.startedAt, Date(timeIntervalSince1970: 910))
    }
}

extension SystemTranscriptionSnapshot {
    fileprivate static let idle = Self(isRunning: false, transcriptFile: nil)
    fileprivate static let running = Self(isRunning: true, transcriptFile: nil)
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

    func read() -> SystemTranscriptionSnapshot {
        lock.withLock {
            reachedCount += 1
            let isRunning = reads.count > 1 ? reads.removeFirst() : reads[0]
            return isRunning ? .running : .idle
        }
    }
}
