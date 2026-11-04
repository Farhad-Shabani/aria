import XCTest

@testable import Aria

final class SpeakerClientTests: XCTestCase {
    private let fallback = "Aria couldn’t save the voice. Try again."

    func testAFailureWithoutItsOwnCopyUsesTheFallback() {
        XCTAssertEqual(SpeakerFailure(.other, fallback: fallback).errorDescription, fallback)
        XCTAssertEqual(SpeakerFailure(.refused(.internal), fallback: fallback).errorDescription, fallback)
    }

    func testAFailureSaysWhenAriaIsNotRunning() {
        let failure = SpeakerFailure(.unavailable, fallback: fallback)
        XCTAssertEqual(failure.errorDescription, "Aria isn’t running. Turn it on in the menu bar.")
    }

    func testADaemonRefusalGetsItsOwnCopy() {
        XCTAssertEqual(
            SpeakerFailure(.refused(.speakerAlreadyEnrolled), fallback: fallback).errorDescription,
            "That name is already in use.")
        XCTAssertEqual(
            SpeakerFailure(.refused(.busy), fallback: fallback).errorDescription,
            "The microphone is in use. Try again when dictation ends.")
    }
}
