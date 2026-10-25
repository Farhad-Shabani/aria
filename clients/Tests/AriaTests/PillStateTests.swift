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
        XCTAssertEqual(PillState(asks: asks), .asking("thinking", since: epoch))

        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 5)
        XCTAssertEqual(PillState(asks: asks), .asking("reading", since: epoch))
    }

    func test_skillStep_carriesTheSkillName() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 1, step: .skill, skill: "pair"), now: epoch)
        XCTAssertEqual(PillState(asks: asks), .asking("skill: pair", since: epoch))
    }

    func test_severalAsks_countTheRestBehindTheOldest() {
        var asks = AsksInFlight()
        asks.apply(event(.working, ticket: 5, agent: .codex, step: .running), now: epoch)
        asks.apply(event(.working, ticket: 3, step: .reading), now: epoch + 1)
        XCTAssertEqual(PillState(asks: asks), .asking("reading · +1", since: epoch + 1))
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
        XCTAssertEqual(PillState.asking("reading", since: epoch).motion, .breathe)
    }

    func test_aDeliveredPressNamesWhereItsTextWent() {
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

    func test_aFocusChangeEndsCalmlyWhetherOrNotTextLanded() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .deliveryTargetChanged
        XCTAssertEqual(PillState(event), .interrupted)
        event.errorCode = .deliveryInterrupted
        XCTAssertEqual(PillState(event), .interrupted)
        XCTAssertEqual(PillState(event)?.label(), "stopped · focus moved")
        XCTAssertEqual(PillState.interrupted.life, .seconds(2))
    }

    func test_anAppWithAriaOffSaysSoByName() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .failed
        event.errorCode = .applicationExcluded

        XCTAssertEqual(PillState(event, frontApp: "1Password")?.label(), "off in 1Password")
        XCTAssertEqual(PillState(event, frontApp: nil)?.label(), "off in this app")
        XCTAssertEqual(PillState.excluded(nil).life, .seconds(2))
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

    func test_aPromptsNameLeadsItsRun_andAnAskNamesNoAgent() {
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

    func test_aLongNameEndsInAnEllipsis() {
        let recording = PillState.recording(since: epoch)
        XCTAssertEqual(recording.label(prompt: "Polish"), "Polish")
        XCTAssertEqual(recording.label(prompt: "Formal email to the leadership team"), "Formal email to the…")
    }

    func test_theRingFollowsTheLastEnrolledVoiceOfThePress() {
        func event(_ stage: Aria_Daemon_V1_DictationStage, speaker: String = "") -> Aria_Daemon_V1_DictationEvent {
            var event = Aria_Daemon_V1_DictationEvent()
            event.stage = stage
            event.speaker = speaker
            return event
        }
        var heard = HeardVoice()

        heard.apply(event(.segmentTranscribed, speaker: "farhad"))
        heard.apply(event(.segmentTranscribed))
        XCTAssertEqual(heard.name, "farhad", "an unmatched segment keeps the ring")

        heard.apply(event(.segmentTranscribed, speaker: "sam"))
        XCTAssertEqual(heard.name, "sam")

        heard.apply(event(.recording))
        XCTAssertNil(heard.name, "a new press starts unheard")

        heard.apply(event(.segmentTranscribed, speaker: "farhad"))
        heard.apply(event(.delivered))
        XCTAssertEqual(heard.name, "farhad", "the result still wears the voice")
        heard.apply(event(.failed))
        XCTAssertNil(heard.name, "a press that fails before it records is unheard")
    }
}
