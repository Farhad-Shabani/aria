import XCTest

@testable import Aria

final class PillStateTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 1_000)

    private func event(
        _ stage: Aria_Daemon_V1_ChatStage, ticket: UInt64,
        assistantID: Aria_Daemon_V1_AssistantID = .claudeCode,
        step: Aria_Daemon_V1_ChatStep = .thinking,
        skill: String = ""
    ) -> Aria_Daemon_V1_ChatEvent {
        var event = Aria_Daemon_V1_ChatEvent()
        event.stage = stage
        event.ticket = ticket
        event.assistantID = assistantID
        event.step = step
        event.skill = skill
        return event
    }

    func testAFailedAskNamesACauseThePersonCanFix() {
        var failed = event(.failed, ticket: 1)
        failed.errorCode = .assistantSignedOut
        XCTAssertEqual(PillState(ended: failed), .failed("Assistant not signed in"))
        failed.errorCode = .assistantUnavailable
        XCTAssertEqual(PillState(ended: failed), .failed("Assistant not installed"))
        failed.errorCode = .promptUnreadable
        XCTAssertEqual(PillState(ended: failed), .failed("can't use its file"))
        failed.errorCode = .assistantFailed
        XCTAssertEqual(PillState(ended: failed), .noReply)
    }

    func testARefusedPromptReadsAsItsFailedRun() {
        let name = "Formal email to the leadership team"
        XCTAssertEqual(
            PillState.refused(prompt: name).label(),
            PillState.failed(PillState.unreadablePrompt).label(prompt: name))
    }

    func testNoAsksNoPill() {
        XCTAssertNil(PillState(asks: AsksInFlight()))
    }

    func testOneAskNamesTheAssistantAndItsStep() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 3), now: epoch)
        XCTAssertEqual(PillState(asks: asks), .asking("thinking", since: epoch))

        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 5)
        XCTAssertEqual(PillState(asks: asks), .asking("reading", since: epoch))
    }

    func testSkillStepCarriesTheSkillName() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1, step: .skill, skill: "pair"), now: epoch)
        XCTAssertEqual(PillState(asks: asks), .asking("skill: pair", since: epoch))
    }

    func testSeveralAsksCountTheRestBehindTheOldest() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 5, assistantID: .codex, step: .running), now: epoch)
        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 1)
        XCTAssertEqual(PillState(asks: asks), .asking("reading · +1", since: epoch + 1))
    }

    func testReplyAndFailureRetireTheirTicket() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1))
        asks.apply(event(.working, ticket: 2))
        asks.apply(event(.replied, ticket: 1))
        XCTAssertEqual(asks.count, 1)
        asks.apply(event(.failed, ticket: 2))
        XCTAssertNil(PillState(asks: asks))
    }

    func testLosingTheStreamClearsEveryAsk() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1))
        asks.apply(nil)
        XCTAssertEqual(asks.count, 0)
    }

    func testMotionPerState() {
        XCTAssertEqual(PillState.recording(since: epoch).motion, .still)
        XCTAssertEqual(PillState.transcribing.motion, .still)
        XCTAssertEqual(PillState.asking("reading", since: epoch).motion, .breathe)
    }

    func testADeliveredPressNamesWhereItsTextWent() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .delivered
        event.outcome = .copied
        event.wordCount = 14
        XCTAssertEqual(PillState(event), .delivered(.copied, words: 14))
        XCTAssertEqual(PillState(event)?.label(), "copied · 14 words")

        event.outcome = .inserted
        event.wordCount = 1
        XCTAssertEqual(PillState(event), .delivered(.inserted, words: 1))
        XCTAssertEqual(PillState(event)?.label(), "1 word")

        // A daemon too old to say, or a press that only wanted its transcript.
        event.outcome = .unspecified
        XCTAssertEqual(PillState(event), .delivered(.unknown, words: 1))
    }

    func testAFocusChangeEndsCalmlyWhetherOrNotTextLanded() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .deliveryTargetChanged
        XCTAssertEqual(PillState(event), .interrupted)
        event.errorCode = .deliveryInterrupted
        XCTAssertEqual(PillState(event), .interrupted)
        XCTAssertEqual(PillState(event)?.label(), "stopped · focus moved")
        XCTAssertEqual(PillState.interrupted.life, .seconds(2))
    }

    func testAnAppWithAriaOffSaysSoByName() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .applicationExcluded

        XCTAssertEqual(PillState(event, frontApp: "1Password")?.label(), "off in 1Password")
        XCTAssertEqual(PillState(event, frontApp: nil)?.label(), "off in this app")
        XCTAssertEqual(PillState.excluded(nil).life, .seconds(2))
    }

    func testASegmentDoesNotChangeThePill() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .segmentTranscribed
        XCTAssertNil(PillState(event))
    }

    func testOnlyAFinishedPressLetsGoOfThePill() {
        XCTAssertNil(PillState.recording(since: epoch).life)
        XCTAssertNil(PillState.transcribing.life)
        XCTAssertNotNil(PillState.delivered(.copied, words: 3).life)
        XCTAssertNotNil(PillState.cancelled.life)
        XCTAssertNotNil(PillState.failed("No speech detected").life)
    }

    func testAPromptsNameLeadsItsRunAndAnAskNamesNoAssistant() {
        let asking = PillState.asking("reading · +1", since: epoch)
        XCTAssertEqual(asking.label(), "reading · +1")
        XCTAssertEqual(PillState.noReply.label(), "no reply")

        XCTAssertEqual(asking.label(prompt: "Polish"), "Polish · reading · +1")
        XCTAssertEqual(PillState.replied(copied: false).label(prompt: "Polish"), "Polish · replied")
        XCTAssertEqual(PillState.replied(copied: true).label(prompt: "Polish"), "Polish · copied")
        XCTAssertEqual(PillState.transcribing.label(prompt: "Polish"), "Polish · transcribing")
        XCTAssertEqual(PillState.recording(since: epoch).label(prompt: "Polish"), "Polish")
        XCTAssertNil(PillState.recording(since: epoch).label())
    }

    func testALongNameEndsInAnEllipsis() {
        let recording = PillState.recording(since: epoch)
        XCTAssertEqual(recording.label(prompt: "Polish"), "Polish")
        XCTAssertEqual(recording.label(prompt: "Formal email to the leadership team"), "Formal email to the…")
    }

    func testOnlyAriasOwnWorkGlows() {
        XCTAssertFalse(Pill.glows(run: .recording(1), ask: nil, prompt: nil), "dictation")
        XCTAssertTrue(Pill.glows(run: .recording(1), ask: .recording(1), prompt: nil), "an ask")
        XCTAssertFalse(Pill.glows(run: .recording(2), ask: .recording(1), prompt: nil), "a later dictation")
        XCTAssertFalse(Pill.glows(run: .recording(1), ask: nil, prompt: "Polish"), "a prompt")
        XCTAssertTrue(Pill.glows(run: .ticket(7), ask: nil, prompt: nil), "an ask's ticket")
        XCTAssertFalse(Pill.glows(run: .ticket(7), ask: nil, prompt: "Polish"), "a prompt's ticket")
        XCTAssertFalse(Pill.glows(run: nil, ask: nil, prompt: nil))
    }
}
