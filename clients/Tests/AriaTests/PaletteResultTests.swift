import XCTest

@testable import Aria

final class PaletteResultTests: XCTestCase {
    private static let input = HistoryItem(
        id: 1, created: Date(timeIntervalSince1970: 0), text: "Standup", status: .success)

    private static func event(_ ticket: UInt64, _ stage: Aria_Daemon_V1_ChatStage, text: String = "")
        -> Aria_Daemon_V1_ChatEvent
    {
        .with {
            $0.ticket = ticket
            $0.stage = stage
            $0.text = text
        }
    }

    private func result(started ticket: UInt64? = nil) -> PaletteResult {
        var result = PaletteResult(promptName: "Summarize", source: .stored(id: "p"), input: Self.input)
        if let ticket { result.started(ticket: ticket) }
        return result
    }

    func testAReplyThatCameBeforeItsTicketIsKeptForIt() {
        var result = result()
        result.apply(Self.event(9, .replied, text: "Not mine."))
        result.apply(Self.event(7, .replied, text: "Short."))

        result.started(ticket: 7)

        XCTAssertEqual(result.state, .replied("Short."))
    }

    func testAReplyShowsAsTheDaemonSentItWhichJudgesItsText() {
        var result = result(started: 7)

        result.apply(Self.event(7, .replied))

        XCTAssertEqual(result.state, .replied(""))
    }

    func testAnotherTicketsEventsLeaveTheRunAlone() {
        var result = result(started: 7)

        result.apply(Self.event(8, .replied, text: "Other."))

        XCTAssertTrue(result.isRunning)
    }

    func testALostStreamFailsARunningPromptButNotAFinishedOne() {
        var running = result(started: 7)
        running.apply(nil)
        XCTAssertEqual(running.state, .failed(.init(PaletteResult.lost)))

        var finished = result(started: 7)
        finished.apply(Self.event(7, .replied, text: "Done."))
        finished.apply(nil)
        XCTAssertEqual(finished.state, .replied("Done."))
    }

    func testAFailureSaysWhatToDoRatherThanTheDaemonsWords() {
        var result = result(started: 7)
        var failed = Self.event(7, .failed)
        failed.errorCode = .assistantSignedOut
        failed.errorMessage = "codex: 401"

        result.apply(failed)

        XCTAssertEqual(result.state, .failed(.init("The assistant is signed out. Sign in, then try again.")))
    }
}
