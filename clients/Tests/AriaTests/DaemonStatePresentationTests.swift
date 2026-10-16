import XCTest

@testable import Aria

final class DaemonStatePresentationTests: XCTestCase {
    func test_statusLines() {
        XCTAssertEqual(DaemonState.running.statusLine, "Running")
        XCTAssertEqual(DaemonState.stopped.statusLine, "Stopped")
        XCTAssertEqual(DaemonState.working.statusLine, "Working…")
    }
}
