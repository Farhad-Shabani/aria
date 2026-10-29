import XCTest

@testable import Aria

final class PressGestureTests: XCTestCase {
    func testOnlyTheFirstPressStartsARunUntilItEnds() {
        var gesture = PressGesture()
        XCTAssertEqual(gesture.press(), .record)
        XCTAssertEqual(gesture.press(), .ignore)
        gesture.issued(ticket: 7)
        XCTAssertEqual(gesture.press(), .ignore)
        gesture.ended(ticket: 7)
        XCTAssertTrue(gesture.isIdle)
        XCTAssertEqual(gesture.press(), .record)
    }

    func testARunThatEndsBeforeItsTicketArrivesFreesTheNextPress() {
        var gesture = PressGesture()
        XCTAssertEqual(gesture.press(), .record)
        gesture.ended(ticket: 8)
        gesture.ended(ticket: 7)
        gesture.issued(ticket: 7)
        XCTAssertTrue(gesture.isIdle)
    }

    func testALostStreamFreesAnAskInFlight() {
        var gesture = PressGesture()
        XCTAssertEqual(gesture.press(), .record)
        gesture.issued(ticket: 1)
        gesture.streamLost()
        XCTAssertTrue(gesture.isIdle)
    }

    func testAStreamLostWhileRecordingDoesNotHoldTheTicket() {
        var gesture = PressGesture()
        XCTAssertEqual(gesture.press(), .record)
        gesture.streamLost()
        XCTAssertFalse(gesture.isIdle, "the recording is still open")
        gesture.issued(ticket: 1)
        XCTAssertTrue(gesture.isIdle, "its end may have passed unseen")
    }
}
