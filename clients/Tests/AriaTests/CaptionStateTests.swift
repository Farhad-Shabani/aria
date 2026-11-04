import XCTest

@testable import Aria

final class CaptionStateTests: XCTestCase {
    private func snapshot(
        _ recording: UInt64 = 1, finalized: String = "", provisional: String = "", ended: Bool = false
    ) -> Aria_Daemon_V1_CaptionSnapshot {
        var snapshot = Aria_Daemon_V1_CaptionSnapshot()
        snapshot.recordingID = recording
        snapshot.finalized = finalized
        snapshot.provisional = provisional
        snapshot.ended = ended
        return snapshot
    }

    private func enabled() -> CaptionState {
        var state = CaptionState()
        state.setEnabled(true)
        return state
    }

    func testRevisionsReplaceAndCommitWithoutDuplication() {
        var state = enabled()
        state.apply(snapshot(provisional: "turn lift here"), now: 0)
        state.apply(snapshot(finalized: "turn", provisional: "left"), now: 1)
        XCTAssertEqual(state.finalized, "turn")
        XCTAssertEqual(state.provisional, "left")
        state.apply(snapshot(finalized: "turn left"), now: 2)
        XCTAssertEqual(state.finalized, "turn left")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(ended: true), now: 3)
        XCTAssertEqual(state.finalized, "")
    }

    func testMissingIntermediateSnapshotRecoversCompleteDisplay() {
        var state = enabled()
        state.apply(snapshot(provisional: "one"), now: 0)
        state.apply(snapshot(finalized: "one two three", provisional: "four"), now: 2)
        XCTAssertEqual(state.finalized, "one two three")
        XCTAssertEqual(state.provisional, "four")
    }

    func testRecordingTransitionsRejectOldUpdatesAndOldEnds() {
        var state = enabled()
        state.apply(snapshot(2, provisional: "preview"), now: 0)
        state.apply(snapshot(3, finalized: "filtered final"), now: 1)
        state.apply(snapshot(2, provisional: "late preview"), now: 2)
        state.apply(snapshot(2, ended: true), now: 3)
        XCTAssertEqual(state.finalized, "filtered final")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(4, provisional: "next preview"), now: 4)
        state.apply(snapshot(3, finalized: "late final"), now: 5)
        XCTAssertEqual(state.finalized, "")
        XCTAssertEqual(state.provisional, "next preview")
    }

    func testEndPreventsResurrectionUntilNewRecording() {
        var state = enabled()
        state.apply(snapshot(8, ended: true), now: 0)
        state.apply(snapshot(8, provisional: "late"), now: 1)
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(9, provisional: "new"), now: 2)
        XCTAssertEqual(state.provisional, "new")
    }

    func testDisconnectClearsAndResetsRecordingIdentity() {
        var state = enabled()
        state.apply(snapshot(8, finalized: "hello", provisional: "world"), now: 0)
        state.apply(nil, now: 1)
        XCTAssertEqual(state.finalized, "")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(1, provisional: "restarted"), now: 2)
        XCTAssertEqual(state.provisional, "restarted")
    }

    func testToggleWaitsForFreshSnapshotAndRetainsEndProtection() {
        var state = enabled()
        state.apply(snapshot(provisional: "hello"), now: 0)
        state.setEnabled(false)
        state.apply(snapshot(provisional: "hidden"), now: 1)
        state.setEnabled(true)
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(provisional: "fresh"), now: 2)
        XCTAssertEqual(state.provisional, "fresh")
        state.setEnabled(false)
        state.apply(snapshot(ended: true), now: 3)
        state.setEnabled(true)
        state.apply(snapshot(provisional: "late"), now: 4)
        XCTAssertEqual(state.provisional, "")
    }

    func testExpiryClearsAfterFiveSecondsAndAllowsFreshUpdate() {
        var state = enabled()
        state.apply(snapshot(finalized: "hello", provisional: "world"), now: 10)
        state.expire(now: 14.999)
        XCTAssertEqual(state.provisional, "world")
        state.expire(now: 15)
        XCTAssertEqual(state.finalized, "")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(finalized: "hello world"), now: 16)
        state.expire(now: 20)
        XCTAssertEqual(state.finalized, "hello world")
    }

    func testIgnoredOldSnapshotDoesNotExtendExpiry() {
        var state = enabled()
        state.apply(snapshot(2, provisional: "current"), now: 0)
        state.apply(snapshot(1, provisional: "old"), now: 4)
        state.expire(now: 5)
        XCTAssertEqual(state.provisional, "")
    }

    func testUnicodeFieldsKeepBoundedNewestScalars() {
        var state = enabled()
        let text = String(repeating: "🙂", count: 2048) + "終"
        state.apply(snapshot(finalized: text, provisional: text), now: 0)
        let expected = String(repeating: "🙂", count: 2047) + "終"
        XCTAssertEqual(state.finalized, expected)
        XCTAssertEqual(state.provisional, expected)
        XCTAssertEqual(state.finalized.unicodeScalars.count, 2048)
    }
}
