import XCTest

@testable import Aria

final class DownloadPaceTests: XCTestCase {
    private let start = Date(timeIntervalSinceReferenceDate: 0)

    func testTheTimeLeftFollowsTheRecentPace() {
        var pace = DownloadPace()
        pace.record(0, at: start)
        pace.record(50, at: start + 5)
        XCTAssertEqual(pace.timeLeft(of: 50 + 1_200 * 10), "About 20 minutes remaining")
    }

    func testAShortSpanGivesNoTime() {
        var pace = DownloadPace()
        pace.record(0, at: start)
        pace.record(50, at: start + 1)
        XCTAssertNil(pace.timeLeft(of: 1_000))
    }

    func testAnOldReadingLeavesThePace() {
        var pace = DownloadPace()
        pace.record(0, at: start)
        pace.record(1_000, at: start + 1)
        pace.record(1_010, at: start + 12)
        pace.record(1_060, at: start + 17)
        XCTAssertEqual(
            pace.timeLeft(of: 1_060 + 600 * 10), "About 10 minutes remaining", "a burst 11 s back no longer counts")
    }

    func testTheLastMinuteIsNotCounted() {
        var pace = DownloadPace()
        pace.record(0, at: start)
        pace.record(100, at: start + 10)
        XCTAssertEqual(pace.timeLeft(of: 300), "Less than a minute remaining")
    }
}
