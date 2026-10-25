import GRPCCore
import XCTest

@testable import Aria

final class SpeakerClientTests: XCTestCase {
    private let fallback = "Aria couldn’t save the voice. Try again."

    func testFailureKeepsDaemonTextOutOfTheMessage() {
        let raw = RPCError(code: .internalError, message: "raw daemon detail")
        XCTAssertEqual(SpeakerClient.failure(raw, fallback: fallback).errorDescription, fallback)
    }

    func testFailureSaysWhenAriaIsNotRunning() {
        let failure = SpeakerClient.failure(RPCError(code: .unavailable, message: ""), fallback: fallback)
        XCTAssertEqual(failure.errorDescription, "Aria isn’t running.")
    }

    func testFailureUsesTheFallbackForAnErrorOutsideGRPC() {
        struct Broken: Error {}
        XCTAssertEqual(SpeakerClient.failure(Broken(), fallback: fallback).errorDescription, fallback)
    }
}
