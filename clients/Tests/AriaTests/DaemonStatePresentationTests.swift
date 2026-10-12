import XCTest

@testable import Aria

final class DaemonStatePresentationTests: XCTestCase {
    func test_markWavesUnlessStopped() {
        XCTAssertEqual(DaemonState.running.markWave, 1)
        XCTAssertEqual(DaemonState.working.markWave, 1)
        XCTAssertEqual(DaemonState.stopped.markWave, 0)
    }

    func test_markDimsWhileWorking() {
        XCTAssertEqual(DaemonState.running.markAlpha, 1)
        XCTAssertEqual(DaemonState.stopped.markAlpha, 1)
        XCTAssertEqual(DaemonState.working.markAlpha, 0.5)
    }

    func test_statusLines() {
        XCTAssertEqual(DaemonState.running.statusLine, "Running")
        XCTAssertEqual(DaemonState.stopped.statusLine, "Stopped")
        XCTAssertEqual(DaemonState.working.statusLine, "Working…")
    }
}
