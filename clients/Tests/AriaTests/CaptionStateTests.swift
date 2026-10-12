import XCTest

@testable import Aria

final class CaptionStateTests: XCTestCase {
    private func snapshot(
        _ recording: UInt64 = 1, committed: String = "", provisional: String = "", ended: Bool = false
    ) -> Aria_Daemon_V1_CaptionSnapshot {
        var snapshot = Aria_Daemon_V1_CaptionSnapshot()
        snapshot.recordingID = recording
        snapshot.committed = committed
        snapshot.provisional = provisional
        snapshot.ended = ended
        return snapshot
    }

    func test_revisions_replaceAndCommitWithoutDuplication() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(provisional: "turn lift here"), now: 0)
        state.apply(snapshot(committed: "turn", provisional: "left"), now: 1)
        XCTAssertEqual(state.committed, "turn")
        XCTAssertEqual(state.provisional, "left")
        state.apply(snapshot(committed: "turn left"), now: 2)
        XCTAssertEqual(state.committed, "turn left")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(ended: true), now: 3)
        XCTAssertEqual(state.committed, "")
    }

    func test_missingIntermediateSnapshot_recoversCompleteDisplay() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(provisional: "one"), now: 0)
        state.apply(snapshot(committed: "one two three", provisional: "four"), now: 2)
        XCTAssertEqual(state.committed, "one two three")
        XCTAssertEqual(state.provisional, "four")
    }

    func test_recordingTransitions_rejectOldUpdatesAndOldEnds() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(2, provisional: "preview"), now: 0)
        state.apply(snapshot(3, committed: "filtered final"), now: 1)
        state.apply(snapshot(2, provisional: "late preview"), now: 2)
        state.apply(snapshot(2, ended: true), now: 3)
        XCTAssertEqual(state.committed, "filtered final")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(4, provisional: "next preview"), now: 4)
        state.apply(snapshot(3, committed: "late final"), now: 5)
        XCTAssertEqual(state.committed, "")
        XCTAssertEqual(state.provisional, "next preview")
    }

    func test_end_preventsResurrectionUntilNewRecording() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(8, ended: true), now: 0)
        state.apply(snapshot(8, provisional: "late"), now: 1)
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(9, provisional: "new"), now: 2)
        XCTAssertEqual(state.provisional, "new")
    }

    func test_disconnect_clearsAndResetsRecordingIdentity() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(8, committed: "hello", provisional: "world"), now: 0)
        state.apply(nil, now: 1)
        XCTAssertEqual(state.committed, "")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(1, provisional: "restarted"), now: 2)
        XCTAssertEqual(state.provisional, "restarted")
    }

    func test_toggle_waitsForFreshSnapshotAndRetainsEndProtection() {
        var state = CaptionState()
        state.setEnabled(true)
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

    func test_expiry_clearsAfterFiveSecondsAndAllowsFreshUpdate() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(committed: "hello", provisional: "world"), now: 10)
        state.expire(now: 14.999)
        XCTAssertEqual(state.provisional, "world")
        state.expire(now: 15)
        XCTAssertEqual(state.committed, "")
        XCTAssertEqual(state.provisional, "")
        state.apply(snapshot(committed: "hello world"), now: 16)
        state.expire(now: 20)
        XCTAssertEqual(state.committed, "hello world")
    }

    func test_ignoredOldSnapshot_doesNotExtendExpiry() {
        var state = CaptionState()
        state.setEnabled(true)
        state.apply(snapshot(2, provisional: "current"), now: 0)
        state.apply(snapshot(1, provisional: "old"), now: 4)
        state.expire(now: 5)
        XCTAssertEqual(state.provisional, "")
    }

    func test_unicodeFields_keepBoundedNewestScalars() {
        var state = CaptionState()
        state.setEnabled(true)
        let text = String(repeating: "🙂", count: 2048) + "終"
        state.apply(snapshot(committed: text, provisional: text), now: 0)
        let expected = String(repeating: "🙂", count: 2047) + "終"
        XCTAssertEqual(state.committed, expected)
        XCTAssertEqual(state.provisional, expected)
        XCTAssertEqual(state.committed.unicodeScalars.count, 2048)
    }
}
