import XCTest

@testable import Aria

final class PillStateTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 1_000)

    private func event(
        _ stage: Aria_Daemon_V1_ChatStage, ticket: UInt64,
        assistantID: Aria_Daemon_V1_AssistantId = .claudeCode,
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

    func testAFailedChatNamesACauseThePersonCanFix() {
        var failed = event(.failed, ticket: 1)
        failed.errorCode = .assistantSignedOut
        XCTAssertEqual(PillState(ended: failed), .failed("assistant signed out"))
        failed.errorCode = .assistantUnavailable
        XCTAssertEqual(PillState(ended: failed), .failed("assistant not installed"))
        failed.errorCode = .promptUnreadable
        XCTAssertEqual(PillState(ended: failed), .failed("file unavailable"))
        failed.errorCode = .assistantFailed
        XCTAssertEqual(PillState(ended: failed), .noReply)
    }

    func testARefusedPromptReadsAsItsFailedRun() {
        let name = "Formal email to the leadership team"
        XCTAssertEqual(
            PillState.refused(prompt: name).label(),
            PillState.failed(PillState.unreadablePrompt).label(prompt: name))
    }

    func testNoChatsNoPill() {
        XCTAssertNil(PillState(chats: ChatsInFlight()))
    }

    func testOneChatNamesTheAssistantAndItsStep() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 3), now: epoch)
        XCTAssertEqual(PillState(chats: chats), .asking("thinking", since: epoch))

        chats.apply(event(.working, ticket: 3, step: .reading), now: epoch + 5)
        XCTAssertEqual(PillState(chats: chats), .asking("reading", since: epoch))
    }

    func testSkillStepCarriesTheSkillName() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 1, step: .skill, skill: "pair"), now: epoch)
        XCTAssertEqual(PillState(chats: chats), .asking("using pair", since: epoch))
    }

    func testSeveralChatsCountTheRestBehindTheOldest() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 5, assistantID: .codex, step: .running), now: epoch)
        chats.apply(event(.working, ticket: 3, step: .reading), now: epoch + 1)
        XCTAssertEqual(PillState(chats: chats), .asking("reading · +1", since: epoch + 1))
    }

    func testAForgottenChatLeavesThePill() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 1))

        chats.forget(1)

        XCTAssertNil(PillState(chats: chats))
    }

    func testReplyAndFailureRetireTheirTicket() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 1))
        chats.apply(event(.working, ticket: 2))
        chats.apply(event(.replied, ticket: 1))
        XCTAssertEqual(chats.count, 1)
        chats.apply(event(.failed, ticket: 2))
        XCTAssertNil(PillState(chats: chats))
    }

    func testLosingTheStreamClearsEveryChat() {
        var chats = ChatsInFlight()
        chats.apply(event(.working, ticket: 1))
        chats.apply(nil)
        XCTAssertEqual(chats.count, 0)
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

        // A daemon too old to say, or a press that only wanted its text.
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

    func testQueuedWordsWaitForTheStartAppUntilTyped() {
        let recording = PillState.recording(since: epoch)

        let notes = StartApp(app: InstalledApp(bundleID: "com.apple.Notes"))
        let waiting = recording.segment(.segmentQueued, startApp: notes)
        XCTAssertEqual(waiting, .recording(since: epoch, waitingFor: notes))
        XCTAssertNil(waiting?.label(), "the start app's icon shows instead")
        let unnamed = StartApp(app: nil)
        XCTAssertEqual(recording.segment(.segmentQueued, startApp: unnamed)?.label(), "waiting")
        XCTAssertEqual(waiting?.segment(.segmentDelivered, startApp: notes), recording)
        XCTAssertNil(PillState.transcribing.segment(.segmentQueued, startApp: notes))
    }

    func testWordsNeverTypedSayWhereTheyAre() {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .delivered
        event.wordCount = 3
        event.outcome = .inHistory
        XCTAssertEqual(PillState(event)?.label(), "saved to history · 3 words")
        XCTAssertEqual(PillState(event)?.life, .seconds(4))
        event.outcome = .notTyped
        XCTAssertEqual(PillState(event)?.label(), "not typed · 3 words")
        XCTAssertEqual(PillState(event)?.life, .seconds(4))
    }

    func testACopiedNoticeStaysLongerThanATypedOne() {
        XCTAssertEqual(PillState.delivered(.copied, words: 3).life, .seconds(4))
        XCTAssertEqual(PillState.replied(copied: true).life, .seconds(4))
        XCTAssertEqual(PillState.delivered(.inserted, words: 3).life, .milliseconds(1200))
    }

    func testOnlyAFinishedPressLetsGoOfThePill() {
        XCTAssertNil(PillState.recording(since: epoch).life)
        XCTAssertNil(PillState.transcribing.life)
        XCTAssertNotNil(PillState.delivered(.copied, words: 3).life)
        XCTAssertNotNil(PillState.cancelled.life)
        XCTAssertNotNil(PillState.failed("no speech").life)
    }

    func testAPromptsNameLeadsItsRunAndAChatNamesNoAssistant() {
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
        XCTAssertFalse(Pill.glows(run: .recording(1), chat: nil, prompt: nil), "dictation")
        XCTAssertTrue(Pill.glows(run: .recording(1), chat: .recording(1), prompt: nil), "a chat")
        XCTAssertFalse(Pill.glows(run: .recording(2), chat: .recording(1), prompt: nil), "a later dictation")
        XCTAssertFalse(Pill.glows(run: .recording(1), chat: nil, prompt: "Polish"), "a prompt")
        XCTAssertTrue(Pill.glows(run: .ticket(7), chat: nil, prompt: nil), "a chat's ticket")
        XCTAssertFalse(Pill.glows(run: .ticket(7), chat: nil, prompt: "Polish"), "a prompt's ticket")
        XCTAssertFalse(Pill.glows(run: nil, chat: nil, prompt: nil))
    }
}
