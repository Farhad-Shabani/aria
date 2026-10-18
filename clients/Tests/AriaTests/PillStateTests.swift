import XCTest

@testable import Aria

final class PillStateTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 1_000)

    private func event(
        _ stage: Aria_Daemon_V1_ChatStage, ticket: UInt64,
        agent: Aria_Daemon_V1_ChatAgent = .claudeCode,
        step: Aria_Daemon_V1_ChatStep = .thinking,
        skill: String = ""
    ) -> Aria_Daemon_V1_ChatEvent {
        var event = Aria_Daemon_V1_ChatEvent()
        event.stage = stage
        event.ticket = ticket
        event.agent = agent
        event.step = step
        event.skill = skill
        return event
    }

    func test_noAsks_noPill() {
        XCTAssertNil(PillState(asks: AsksInFlight()))
    }

    func test_oneAsk_namesTheAgentAndItsStep() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 3), now: epoch)
        XCTAssertEqual(PillState(asks: asks), .asking("claude · thinking", since: epoch))

        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 5)
        XCTAssertEqual(PillState(asks: asks), .asking("claude · reading", since: epoch))
    }

    func test_skillStep_carriesTheSkillName() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1, step: .skill, skill: "pair"), now: epoch)
        XCTAssertEqual(PillState(asks: asks), .asking("claude · skill: pair", since: epoch))
    }

    func test_severalAsks_countTheRestBehindTheOldest() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 5, agent: .codex, step: .running), now: epoch)
        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 1)
        XCTAssertEqual(PillState(asks: asks), .asking("claude · reading · +1", since: epoch + 1))
    }

    func test_replyAndFailure_retireTheirTicket() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1))
        asks.apply(event(.working, ticket: 2))
        asks.apply(event(.replied, ticket: 1))
        XCTAssertEqual(asks.count, 1)
        asks.apply(event(.failed, ticket: 2))
        XCTAssertNil(PillState(asks: asks))
    }

    func test_losingTheStream_clearsEveryAsk() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1))
        asks.apply(nil)
        XCTAssertEqual(asks.count, 0)
    }

    func test_motionPerState() {
        XCTAssertEqual(PillState.recording(since: epoch).motion, .still)
        XCTAssertEqual(PillState.transcribing.motion, .still)
        XCTAssertEqual(PillState.asking("claude · reading", since: epoch).motion, .breathe)
    }

    func test_aDeliveredPressNamesWhereItsTextWent() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .delivered
        event.outcome = .copied
        event.wordCount = 14
        XCTAssertEqual(PillState(event), .delivered(.copied, words: 14))
        XCTAssertEqual(PillState(event)?.label, "copied · 14 words")

        event.outcome = .inserted
        event.wordCount = 1
        XCTAssertEqual(PillState(event), .delivered(.inserted, words: 1))
        XCTAssertEqual(PillState(event)?.label, "1 word")

        // A daemon too old to say, or a press that only wanted its transcript.
        event.outcome = .unspecified
        XCTAssertEqual(PillState(event), .delivered(.unknown, words: 1))
    }

    func test_targetChangeDoesNotClaimEarlierTextWasUndelivered() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .deliveryTargetChanged
        XCTAssertEqual(PillState(event), .failed("The target changed — insertion stopped"))
    }

    func test_typingStoppedPartwayEndsCalmly() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .deliveryInterrupted
        XCTAssertEqual(PillState(event), .interrupted)
        XCTAssertEqual(PillState(event)?.label, "stopped · focus moved")
        XCTAssertEqual(PillState.interrupted.life, .seconds(2))
    }

    func test_aSegmentDoesNotChangeThePill() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .segmentTranscribed
        XCTAssertNil(PillState(event))
    }

    func test_onlyAFinishedPressLetsGoOfThePill() {
        XCTAssertNil(PillState.recording(since: epoch).life)
        XCTAssertNil(PillState.transcribing.life)
        XCTAssertNotNil(PillState.delivered(.copied, words: 3).life)
        XCTAssertNotNil(PillState.cancelled.life)
        XCTAssertNotNil(PillState.failed("No speech detected").life)
    }

    func test_agentShortNames() {
        XCTAssertEqual(Aria_Daemon_V1_ChatAgent.claudeCode.shortName, "claude")
        XCTAssertEqual(Aria_Daemon_V1_ChatAgent.codex.shortName, "codex")
    }
}
