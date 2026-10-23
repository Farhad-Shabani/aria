import XCTest

@testable import Aria

final class MenuBarStateTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 1_000)

    private func state(_ daemon: DaemonState = .running, pill: PillState? = nil) -> MenuBarState {
        MenuBarState(daemon: daemon, pill: pill)
    }

    func test_theDaemonComesFirst() {
        XCTAssertEqual(state(.stopped, pill: .recording(since: epoch)), .stopped)
        XCTAssertEqual(state(.working, pill: .recording(since: epoch)), .switching)
    }

    func test_aRunningDaemonFollowsThePill() {
        XCTAssertEqual(state(pill: .recording(since: epoch)), .listening)
        XCTAssertEqual(state(pill: .transcribing), .working)
        XCTAssertEqual(state(pill: .delivering), .working)
        XCTAssertEqual(state(pill: .asking("claude · reading", since: epoch)), .working)
        XCTAssertEqual(state(pill: .delivered(.inserted, words: 3)), .done)
        XCTAssertEqual(state(pill: .delivered(.copied, words: 3)), .done)
        XCTAssertEqual(state(pill: .replied("claude")), .done)
    }

    func test_aFailureNeedsThePerson() {
        XCTAssertEqual(state(pill: .failed("Dictation failed")), .attention)
        XCTAssertEqual(state(pill: .noReply("claude")), .attention)
    }

    func test_idleIsReady() {
        XCTAssertEqual(state(), .ready)
        XCTAssertEqual(state(pill: .cancelled), .ready)
        XCTAssertEqual(state(pill: .interrupted), .ready)
        XCTAssertEqual(state(pill: .excluded("Slack")), .ready)
    }
}
