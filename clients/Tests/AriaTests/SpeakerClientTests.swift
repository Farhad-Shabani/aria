import GRPCCore
import XCTest

@testable import Aria

final class SpeakerClientTests: XCTestCase {
    private let fallback = "Aria couldn’t save the voice. Try again."

    func test_failure_keepsDaemonTextOutOfTheMessage() {
        let raw = RPCError(code: .internalError, message: "raw daemon detail")
        XCTAssertEqual(SpeakerClient.failure(raw, fallback: fallback).errorDescription, fallback)
    }

    func test_failure_saysWhenAriaIsNotRunning() {
        let failure = SpeakerClient.failure(RPCError(code: .unavailable, message: ""), fallback: fallback)
        XCTAssertEqual(failure.errorDescription, "Aria isn’t running.")
    }

    func test_failure_usesTheFallbackForAnErrorOutsideGRPC() {
        struct Broken: Error {}
        XCTAssertEqual(SpeakerClient.failure(Broken(), fallback: fallback).errorDescription, fallback)
    }
}
