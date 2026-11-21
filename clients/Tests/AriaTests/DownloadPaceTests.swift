import XCTest

@testable import Aria

final class DownloadPaceTests: XCTestCase {
    /// Each reading is the bytes received by that many seconds in.
    private func pace(_ readings: (received: UInt64, second: TimeInterval)...) -> DownloadPace {
        let start = Date(timeIntervalSinceReferenceDate: 0)
        var pace = DownloadPace()
        for reading in readings { pace.record(reading.received, at: start + reading.second) }
        return pace
    }

    func testTheTimeLeftFollowsTheRecentPace() {
        XCTAssertEqual(pace((0, 0), (50, 5)).timeLeft(of: 50 + 1_200 * 10), "About 20 minutes remaining")
    }

    func testAShortSpanGivesNoTime() {
        XCTAssertNil(pace((0, 0), (50, 1)).timeLeft(of: 1_000))
    }

    func testAnOldReadingLeavesThePace() {
        XCTAssertEqual(
            pace((0, 0), (1_000, 1), (1_010, 12), (1_060, 17)).timeLeft(of: 1_060 + 600 * 10),
            "About 10 minutes remaining", "a burst 11 s back no longer counts")
    }

    func testTheLastMinuteIsNotCounted() {
        XCTAssertEqual(pace((0, 0), (100, 10)).timeLeft(of: 300), "Less than a minute remaining")
    }
}
