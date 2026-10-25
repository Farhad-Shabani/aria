import XCTest

@testable import Aria

final class DaemonStatePresentationTests: XCTestCase {
    func testStatusLines() {
        XCTAssertEqual(DaemonState.running.statusLine, "Running")
        XCTAssertEqual(DaemonState.stopped.statusLine, "Stopped")
        XCTAssertEqual(DaemonState.working.statusLine, "Working…")
    }
}
