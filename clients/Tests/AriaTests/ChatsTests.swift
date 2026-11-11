import XCTest

@testable import Aria

@MainActor
final class ChatsTests: XCTestCase {
    private struct Asked: Equatable {
        let text: String
        let chat: ChatKey
        let folder: String?
        let ownVoice: Bool
        let tapCancels: Bool
    }

    private struct Started: Equatable {
        let text: String
        var quote: String? = nil
        let workdir: String
        let assistantID: Aria_Daemon_V1_AssistantId
        let ownVoice: Bool
    }

    @MainActor
    private final class Harness {
        var started: [Started] = []
        var starts: [CheckedContinuation<StartedChat, Error>] = []
        var asked: [Asked] = []
        var tickets: [CheckedContinuation<UInt64, Error>] = []
        var page: CheckedContinuation<ChatPage, Error>?
        var cancelled: [ChatKey] = []
        var cancelFailure: DaemonFailure?
        var reads: [UInt64?] = []
        var holdsLists = false
        var resolves: [CheckedContinuation<ChatTarget, Error>] = []
        var holdsResolve = false
        var lists: [CheckedContinuation<[Aria_Daemon_V1_PendingAsk], Error>] = []

        var effects: Chats.Effects {
            .init(
                resolve: {
                    guard self.holdsResolve else { return ChatsTests.target }
                    return try await withCheckedThrowingContinuation { self.resolves.append($0) }
                },
                start: { text, quote, target, ownVoice in
                    self.started.append(
                        Started(
                            text: text, quote: quote, workdir: target.workdir, assistantID: target.assistantID,
                            ownVoice: ownVoice))
                    return try await withCheckedThrowingContinuation { self.starts.append($0) }
                },
                ask: { text, chat, folder, ownVoice, tapCancels in
                    self.asked.append(
                        Asked(text: text, chat: chat, folder: folder, ownVoice: ownVoice, tapCancels: tapCancels))
                    return try await withCheckedThrowingContinuation { self.tickets.append($0) }
                },
                cancel: {
                    self.cancelled.append($0)
                    if let failure = self.cancelFailure { throw failure }
                },
                read: { _, before in
                    self.reads.append(before)
                    return try await withCheckedThrowingContinuation { self.page = $0 }
                },
                pending: { _ in
                    guard self.holdsLists else { return [] }
                    return try await withCheckedThrowingContinuation { self.lists.append($0) }
                })
        }
    }

    private static let target = ChatTarget(workdir: "/code/aria", assistantID: .claudeCode)
    private static let key = ChatKey(assistantID: .claudeCode, name: "minted")

    private static func named(_ ticket: UInt64) -> StartedChat {
        StartedChat(ticket: ticket, chat: key)
    }

    private static func event(
        _ ticket: UInt64, _ stage: Aria_Daemon_V1_ChatStage, text: String = "", entry: UInt64? = nil
    ) -> Aria_Daemon_V1_ChatEvent {
        .with {
            $0.ticket = ticket
            $0.stage = stage
            $0.text = text
            if let entry { $0.historyEntryID = entry }
        }
    }

    private static func pending(_ ticket: UInt64, _ text: String, running: Bool) -> Aria_Daemon_V1_PendingAsk {
        .with {
            $0.ticket = ticket
            $0.text = text
            $0.running = running
        }
    }

    private static func saved(_ id: UInt64, at seconds: TimeInterval, question: String? = nil) -> HistoryItem {
        HistoryItem(
            id: id, kind: .ask(reply: "Reply \(id)"), created: Date(timeIntervalSince1970: seconds),
            project: "/code/aria", chat: key, text: question ?? "Question \(id)", status: .success)
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<200 where !condition() { await Task.yield() }
        XCTAssertTrue(condition(), file: file, line: line)
    }

    private func starting(_ harness: Harness) async -> Chats {
        let chats = Chats(effects: harness.effects)
        chats.startNew("When is lunch?", at: Self.target, ownVoice: true)
        await waitUntil { harness.starts.count == 1 }
        return chats
    }

    /// A chat started on “Launch notes” from history entry `entry`, its start still out.
    private func startingQuote(_ harness: Harness, from entry: UInt64) async -> Chats {
        let chats = Chats(effects: harness.effects)
        await chats.startNew("Make it shorter", quoting: "Launch notes", from: entry)
        await waitUntil { harness.starts.count == 1 }
        return chats
    }

    private func opened(_ harness: Harness, keepsHistory: Bool) -> Chats {
        let chats = Chats(effects: harness.effects)
        chats.keepsHistory = keepsHistory
        chats.open(Self.key, folder: "/code/aria")
        return chats
    }

    private func started(_ harness: Harness) async -> Chats {
        let chats = Chats(effects: harness.effects)
        chats.startNew("When is lunch?", at: Self.target, ownVoice: true)
        await waitUntil { harness.starts.count == 1 }
        harness.starts[0].resume(returning: Self.named(7))
        await waitUntil { chats.shown?.asks.first?.ticket == 7 }
        return chats
    }

    func testANewAskShowsAtOnceAndTakesItsKeyFromTheReply() async {
        let harness = Harness()
        let chats = Chats(effects: harness.effects)

        chats.startNew("When is lunch?", at: Self.target, ownVoice: true)

        XCTAssertNil(chats.shown?.key, "the daemon names it")
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["When is lunch?"])
        XCTAssertEqual(chats.shown?.asks.first?.state, .running("Starting…"))
        await waitUntil { harness.started.count == 1 }
        XCTAssertEqual(
            harness.started,
            [Started(text: "When is lunch?", workdir: "/code/aria", assistantID: .claudeCode, ownVoice: true)])

        harness.starts[0].resume(returning: Self.named(7))

        await waitUntil { chats.shown?.key == Self.key }
        XCTAssertEqual(chats.shown?.asks.first?.ticket, 7)
    }

    func testAChatTheDaemonNeverNamedTakesNoFollowUpAndAskStartsAnother() async {
        let harness = Harness()
        let chats = await starting(harness)

        harness.starts[0].resume(throwing: DaemonFailure.invalidResponse)

        await waitUntil { chats.shown?.isReplying == false }
        XCTAssertEqual(chats.shown?.asks.map(\.state), [.failed(ChatAsk.failed)])
        XCTAssertNil(chats.shown?.key)
        XCTAssertFalse(chats.send("And dinner?"))
        XCTAssertEqual(AskTarget(shown: chats.shown), .new)
    }

    func testEventsThatComeBeforeTheTicketApplyOnceItArrives() async {
        let harness = Harness()
        let chats = await starting(harness)

        chats.apply(Self.event(9, .replied, text: "Not mine."))
        chats.apply(Self.event(7, .replied, text: "Noon.", entry: 4))
        harness.starts[0].resume(returning: Self.named(7))

        await waitUntil { chats.shown?.asks.first?.state == .replied("Noon.") }
        XCTAssertEqual(chats.shown?.asks.first?.entryID, 4)
        XCTAssertTrue(chats.shows(ticket: 7))
    }

    func testAFailedCallFailsOnlyItsAsk() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.apply(Self.event(7, .replied, text: "Noon."))

        XCTAssertTrue(chats.send("And dinner?"))
        await waitUntil { harness.tickets.count == 1 }
        harness.tickets[0].resume(throwing: DaemonFailure.refused(.busy))

        await waitUntil { chats.shown?.asks.last?.isRunning == false }
        XCTAssertEqual(
            chats.shown?.asks.map(\.state),
            [.replied("Noon."), .failed("Aria is busy. Ask again in a moment.")])
    }

    func testALostStreamLosesOnlyTheAsksStillRunning() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.apply(Self.event(7, .replied, text: "Noon."))
        XCTAssertTrue(chats.send("And dinner?"))

        chats.apply(nil)

        XCTAssertEqual(chats.shown?.asks.map(\.state), [.replied("Noon."), .lost])
        await waitUntil { harness.tickets.count == 1 }
        harness.tickets[0].resume(returning: 8)
    }

    func testOpeningShowsTheAsksTheDaemonStillRunsAfterTheSavedOnes() async {
        let harness = Harness()
        harness.holdsLists = true
        let chats = opened(harness, keepsHistory: true)
        await waitUntil { harness.lists.count == 1 && harness.page != nil }

        harness.lists[0].resume(
            returning: [Self.pending(8, "And dinner?", running: true), Self.pending(9, "And tea?", running: false)])
        await waitUntil { chats.shown?.asks.count == 2 }
        harness.page?.resume(
            returning: ChatPage(asks: [Self.saved(4, at: 0), Self.saved(5, at: 1)], older: nil))

        await waitUntil { chats.shown?.asks.count == 4 }
        XCTAssertEqual(
            chats.shown?.asks.map(\.question), ["Question 4", "Question 5", "And dinner?", "And tea?"])
        XCTAssertEqual(chats.shown?.asks.suffix(2).map(\.state), [.running("Working…"), .running("Starting…")])
        XCTAssertTrue(chats.shows(ticket: 9))
        chats.apply(Self.event(8, .replied, text: "Seven."))
        XCTAssertEqual(chats.shown?.asks[2].state, .replied("Seven."))
    }

    func testWithHistoryOffOpeningStillShowsTheAsksTheDaemonRuns() async {
        let harness = Harness()
        harness.holdsLists = true
        let chats = opened(harness, keepsHistory: false)
        await waitUntil { harness.lists.count == 1 }

        harness.lists[0].resume(returning: [Self.pending(8, "And dinner?", running: true)])

        await waitUntil { chats.shown?.isReplying == true }
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["And dinner?"])
        XCTAssertEqual(harness.reads, [])
    }

    func testAnAskThatRepliesWhileItIsListedShowsItsReply() async {
        let harness = Harness()
        harness.holdsLists = true
        let chats = opened(harness, keepsHistory: false)
        await waitUntil { harness.lists.count == 1 }

        chats.apply(Self.event(8, .replied, text: "Seven."))
        harness.lists[0].resume(returning: [Self.pending(8, "And dinner?", running: true)])

        await waitUntil { chats.shown?.asks.first?.state == .replied("Seven.") }
    }

    func testAnAskTheDaemonListsBeforeItsTicketComesShowsOnce() async {
        let harness = Harness()
        harness.holdsLists = true
        let chats = opened(harness, keepsHistory: false)
        await waitUntil { harness.lists.count == 1 }
        XCTAssertTrue(chats.send("And dinner?"))
        await waitUntil { harness.tickets.count == 1 }
        harness.lists[0].resume(returning: [Self.pending(8, "And dinner?", running: true)])
        await waitUntil { chats.shown?.asks.count == 2 }

        harness.tickets[0].resume(returning: 8)

        await waitUntil { chats.shown?.asks.count == 1 }
        XCTAssertEqual(chats.shown?.asks.first?.id, .live(1))
        XCTAssertEqual(chats.shown?.asks.first?.ticket, 8)
    }

    func testReconnectingRevivesALostAskTheDaemonStillRuns() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.keepsHistory = true
        chats.apply(nil)
        harness.holdsLists = true

        chats.reconnected()
        await waitUntil { harness.lists.count == 1 }
        harness.lists[0].resume(returning: [Self.pending(7, "When is lunch?", running: true)])

        await waitUntil { chats.shown?.isReplying == true }
        chats.apply(Self.event(7, .replied, text: "Noon."))
        XCTAssertEqual(chats.shown?.asks.map(\.state), [.replied("Noon.")])
        XCTAssertEqual(harness.reads, [], "nothing ended, so nothing is read")
    }

    func testReconnectingReadsAgainALostAskThatEndedMeanwhile() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.keepsHistory = true
        chats.apply(nil)

        chats.reconnected()
        await waitUntil { harness.reads == [nil] }
        harness.page?.resume(
            returning: ChatPage(asks: [Self.saved(4, at: 0, question: "When is lunch?")], older: nil))

        await waitUntil { chats.shown?.asks.map(\.id) == [.saved(4)] }
        XCTAssertEqual(chats.shown?.asks.map(\.state), [.replied("Reply 4")])
    }

    func testALostAskThatEndedWithNoSavedCopyStaysLost() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.keepsHistory = true
        chats.apply(nil)

        chats.reconnected()
        await waitUntil { harness.reads == [nil] }
        harness.page?.resume(returning: ChatPage(asks: [Self.saved(4, at: 0)], older: nil))

        await waitUntil { chats.shown?.asks.count == 2 }
        XCTAssertEqual(chats.shown?.asks.map(\.id), [.saved(4), .live(1)])
        XCTAssertEqual(chats.shown?.asks.map(\.state), [.replied("Reply 4"), .lost])
    }

    func testSavedAsksMergeWithTheOnesAskedHereByTheirEntry() async {
        let harness = Harness()
        let chats = opened(harness, keepsHistory: true)
        await waitUntil { harness.page != nil }
        XCTAssertTrue(chats.send("And dinner?"))
        await waitUntil { harness.tickets.count == 1 }
        harness.tickets[0].resume(returning: 7)
        await waitUntil { chats.shown?.asks.last?.ticket == 7 }
        chats.apply(Self.event(7, .replied, text: "Seven.", entry: 5))

        harness.page?.resume(
            returning: ChatPage(asks: [Self.saved(4, at: 0), Self.saved(5, at: 1)], older: 4, quote: "Launch notes"))

        await waitUntil { chats.shown?.asks.count == 2 }
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["Question 4", "And dinner?"])
        XCTAssertEqual(chats.shown?.asks.map(\.entryID), [4, 5])
        XCTAssertEqual(chats.shown?.older, 4)
        XCTAssertEqual(chats.shown?.quote, "Launch notes", "the chat's quote comes with the read")
    }

    func testASavedCopyReadBeforeTheReplyGivesWayToTheAskAskedHere() async {
        let harness = Harness()
        let chats = opened(harness, keepsHistory: true)
        await waitUntil { harness.page != nil }
        XCTAssertTrue(chats.send("And dinner?"))
        await waitUntil { harness.tickets.count == 1 }
        harness.tickets[0].resume(returning: 7)
        await waitUntil { chats.shown?.asks.last?.ticket == 7 }

        harness.page?.resume(
            returning: ChatPage(asks: [Self.saved(4, at: 0), Self.saved(5, at: 1)], older: nil))
        await waitUntil { chats.shown?.asks.count == 3 }
        chats.apply(Self.event(7, .replied, text: "Seven.", entry: 5))

        XCTAssertEqual(chats.shown?.asks.map(\.question), ["Question 4", "And dinner?"])
        XCTAssertEqual(chats.shown?.asks.map(\.entryID), [4, 5])
        XCTAssertEqual(chats.shown?.isReplying, false)
    }

    func testStopStopsTheChatInTheDaemonButNotBeforeItIsNamed() async {
        let harness = Harness()
        let chats = await starting(harness)

        chats.stop(Self.key)
        XCTAssertEqual(chats.shown?.canStop, false)
        harness.starts[0].resume(returning: Self.named(7))
        await waitUntil { chats.shown?.canStop == true }
        XCTAssertEqual(harness.cancelled, [], "a stop before the key stops nothing")

        chats.stop(Self.key)

        await waitUntil { harness.cancelled == [Self.key] }
    }

    func testAStopThatFailsSaysSoUntilTheAskEnds() async {
        let harness = Harness()
        harness.cancelFailure = .invalidResponse
        let chats = await started(harness)

        chats.stop(Self.key)

        await waitUntil { chats.lastStop?.failure != nil }
        XCTAssertEqual(chats.lastStop?.failure, Chats.stopFailed)
        chats.apply(Self.event(7, .replied, text: "Noon."))
        XCTAssertNil(chats.lastStop?.failure, "the ask it meant to stop ended")
    }

    func testStopReachesAChatClosedWhileItRepliesAndSaysWhenItFails() async {
        let harness = Harness()
        harness.cancelFailure = .invalidResponse
        let chats = await started(harness)
        chats.close()

        chats.stop(Self.key)

        await waitUntil { chats.lastStop?.failure != nil }
        XCTAssertEqual(harness.cancelled, [Self.key])
        XCTAssertEqual(chats.lastStop?.failure, Chats.stopFailed)
        chats.apply(Self.event(7, .failed))
        XCTAssertNil(chats.lastStop?.failure, "the ask it meant to stop ended")
    }

    func testAnEarlierReadThatFailedReadsTheSamePageAgain() async {
        let harness = Harness()
        let chats = opened(harness, keepsHistory: true)
        await waitUntil { harness.reads.count == 1 }
        harness.page?.resume(returning: ChatPage(asks: [Self.saved(5, at: 1)], older: 5))
        await waitUntil { chats.shown?.older == 5 }
        chats.readEarlier()
        await waitUntil { harness.reads.count == 2 }
        harness.page?.resume(throwing: HistoryFailure.storage)
        await waitUntil { chats.error == .storage }

        chats.readEarlier()
        XCTAssertEqual(harness.reads.count, 2, "only Try Again reads after a failure")
        chats.retryRead()

        await waitUntil { harness.reads.count == 3 }
        XCTAssertEqual(harness.reads, [nil, 5, 5])
        XCTAssertNil(chats.error)
        harness.page?.resume(returning: ChatPage(asks: [Self.saved(4, at: 0)], older: nil))
        await waitUntil { chats.shown?.asks.count == 2 }
        XCTAssertEqual(chats.shown?.asks.map(\.entryID), [4, 5])
        XCTAssertNil(chats.shown?.older)
    }

    func testATypedFollowUpWaitsForTheLastAskAndNeverCancelsOnATap() async {
        let harness = Harness()
        let chats = await started(harness)

        XCTAssertFalse(chats.send("And dinner?"), "the first ask is still replying")
        chats.apply(Self.event(7, .replied, text: "Noon."))
        XCTAssertFalse(chats.canSend(" \n"), "nothing but spaces")
        XCTAssertTrue(chats.canSend("And dinner?"))
        XCTAssertTrue(chats.send("And dinner?"))

        await waitUntil { harness.asked.count == 1 }
        XCTAssertEqual(harness.asked.last?.chat, Self.key)
        XCTAssertEqual(harness.asked.last?.folder, "/code/aria", "the folder Resolve gave the chat")
        XCTAssertEqual(harness.asked.last?.ownVoice, false)
        XCTAssertEqual(harness.asked.last?.tapCancels, false)
        harness.tickets[0].resume(returning: 8)
    }

    func testAFollowUpOnATextStartsAChatThatShowsAndSendsItsQuote() async {
        let harness = Harness()
        let chats = Chats(effects: harness.effects)

        await chats.startNew(" Make it shorter \n", quoting: "Launch notes", from: 2)

        XCTAssertEqual(chats.shown?.folder, "/code/aria", "the folder Resolve gave")
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["Make it shorter"])
        XCTAssertEqual(chats.shown?.quote, "Launch notes")
        await waitUntil { harness.started.count == 1 }
        XCTAssertEqual(
            harness.started,
            [
                Started(
                    text: "Make it shorter", quote: "Launch notes", workdir: "/code/aria", assistantID: .claudeCode,
                    ownVoice: false)
            ])
        harness.starts[0].resume(returning: Self.named(7))
    }

    func testAFollowUpOnATextCancelledWhileItsFolderResolvesAsksNothing() async {
        let harness = Harness()
        harness.holdsResolve = true
        let chats = Chats(effects: harness.effects)
        let start = Task { await chats.startNew("Make it shorter", quoting: "Launch notes", from: 2) }
        await waitUntil { harness.resolves.count == 1 }

        start.cancel()
        harness.resolves[0].resume(returning: Self.target)
        await start.value

        XCTAssertNil(chats.shown)
        XCTAssertTrue(harness.started.isEmpty)
    }

    func testAChatClosedWhileItRepliesFollowsItsAskUntilTheReply() async {
        let harness = Harness()
        let chats = await started(harness)

        chats.close()
        chats.apply(Self.event(7, .working))

        XCTAssertNil(chats.shown)
        XCTAssertEqual(chats.followed.map(\.key), [Self.key])
        chats.apply(Self.event(7, .replied, text: "Noon.", entry: 4))
        XCTAssertTrue(chats.followed.isEmpty, "it goes once nothing in it runs")
        XCTAssertEqual(chats.lastOutcome?.announcement, "Reply ready", "VoiceOver hears it on the list")
    }

    func testAStartThatAnswersAfterItsChatClosedStillNamesItAndTakesItsTicket() async {
        let harness = Harness()
        let chats = await starting(harness)

        chats.close()
        harness.starts[0].resume(returning: Self.named(7))

        await waitUntil { chats.followed.first?.asks.first?.ticket == 7 }
        XCTAssertEqual(chats.followed.first?.key, Self.key)
        chats.apply(Self.event(7, .failed))
        XCTAssertTrue(chats.followed.isEmpty, "its ticket's events reach it")
    }

    func testAQuotedChatClosedWhileItRepliesShowsOnTheEntryItQuotes() async {
        let harness = Harness()
        let chats = await startingQuote(harness, from: 2)
        harness.starts[0].resume(returning: Self.named(7))
        await waitUntil { chats.shown?.asks.first?.ticket == 7 }

        chats.close()

        XCTAssertNotNil(chats.replying(quoting: 2))
        XCTAssertNil(chats.replying(quoting: 3))
        XCTAssertNotNil(chats.replying(Self.key))
        chats.apply(Self.event(7, .failed))
        XCTAssertNil(chats.replying(quoting: 2), "a failed ask no longer replies")
    }

    func testReopeningAQuotedChatBeforeItsNameComesShowsItsQuestion() async {
        let harness = Harness()
        let chats = await startingQuote(harness, from: 2)
        chats.close()

        XCTAssertTrue(chats.open(quoting: 2))

        XCTAssertNil(chats.shown?.key)
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["Make it shorter"])
        XCTAssertEqual(chats.shown?.quote, "Launch notes")
        harness.starts[0].resume(returning: Self.named(7))
        await waitUntil { chats.shown?.key == Self.key }
    }

    func testAClosedChatWhoseStreamDroppedStaysUntilTheDaemonListsItsAskAgain() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.close()
        harness.holdsLists = true

        chats.apply(nil)
        XCTAssertEqual(chats.followed.map(\.key), [Self.key], "lost, not ended")
        chats.reconnected()
        await waitUntil { harness.lists.count == 1 }
        harness.lists[0].resume(returning: [Self.pending(7, "When is lunch?", running: true)])

        await waitUntil { chats.replying(Self.key) != nil }
    }

    func testAClosedChatWhoseLostAskEndedGoesOnceTheDaemonListsNothing() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.close()
        harness.holdsLists = true
        chats.apply(nil)

        chats.reconnected()
        await waitUntil { harness.lists.count == 1 }
        harness.lists[0].resume(returning: [])

        await waitUntil { chats.followed.isEmpty }
    }

    func testAStopThatFailsAfterItsPageClosedStillSaysSo() async {
        let harness = Harness()
        harness.cancelFailure = .invalidResponse
        let chats = await started(harness)

        chats.stop(Self.key)
        chats.close()

        await waitUntil { chats.lastStop?.failure != nil }
        XCTAssertEqual(chats.lastStop?.failure, Chats.stopFailed)
    }

    func testAStopFailureShowsOnlyWhereItsChatDoes() async {
        let harness = Harness()
        harness.cancelFailure = .invalidResponse
        let chats = await started(harness)
        chats.close()

        chats.stop(Self.key)
        await waitUntil { chats.lastStop?.failure != nil }
        chats.open(ChatKey(assistantID: .claudeCode, name: "other"), folder: "/code/aria")

        XCTAssertNil(chats.lastStop, "another chat's page")
    }

    func testADeletedChatLeavesNothingBehind() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.close()
        chats.apply(nil)
        XCTAssertEqual(chats.followed.map(\.key), [Self.key], "lost, waiting to be listed")

        chats.deleted(Self.key)

        XCTAssertTrue(chats.followed.isEmpty)
    }

    func testOnlyTheAskThatQuotedAnEntryShowsOnThatEntry() async {
        let harness = Harness()
        let chats = await startingQuote(harness, from: 2)
        harness.starts[0].resume(returning: Self.named(7))
        await waitUntil { chats.shown?.asks.first?.ticket == 7 }
        chats.apply(Self.event(7, .replied, text: "Shorter.", entry: 4))

        XCTAssertTrue(chats.send("And the title?"))
        chats.close()

        XCTAssertNotNil(chats.replying(Self.key))
        XCTAssertNil(chats.replying(quoting: 2), "the follow-up quoted nothing")
        harness.tickets.first?.resume(returning: 8)
    }

    func testOpeningAChatClosedWhileItRepliesShowsItsRunningAsk() async {
        let harness = Harness()
        let chats = await started(harness)
        chats.close()

        chats.open(Self.key, folder: "/code/aria")

        XCTAssertEqual(chats.followed.count, 1, "followed once, not twice")
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["When is lunch?"])
        XCTAssertEqual(chats.shown?.asks.first?.ticket, 7)
        XCTAssertEqual(chats.shown?.isReplying, true)
    }

    func testAPromptResultSavedOnAnAskCountsOnIt() {
        var chat = Chat(id: 1, key: Self.key, folder: "/code/aria")
        chat.merge(ChatPage(asks: [Self.saved(4, at: 0), Self.saved(5, at: 1)], older: nil))

        chat.countResult(on: 5)
        chat.countResult(on: 9)

        XCTAssertEqual(chat.asks.map(\.resultCount), [0, 1])
    }

    func testAShownChatTakesTheNextQuestionUnlessItIsReplying() {
        var chat = Chat(id: 1, key: Self.key, folder: "/code/aria")
        XCTAssertEqual(AskTarget(shown: chat), .followUp(Self.key))

        chat.append(ChatAsk(live: 1, question: "When is lunch?"))

        XCTAssertNil(AskTarget(shown: chat))
    }

    func testWithNoChatShownTheQuestionStartsOne() {
        XCTAssertEqual(AskTarget(shown: nil), .new)
    }

    func testARenameMovesTheShownChatAndItsRecordingToTheNewName() {
        let chats = opened(Harness(), keepsHistory: false)
        let renamed = ChatKey(assistantID: .claudeCode, name: "Lunch plans")
        chats.recording = Self.key

        chats.renamed(
            ChatKey(assistantID: .claudeCode, name: "other"), to: ChatKey(assistantID: .claudeCode, name: "ignored"))
        XCTAssertEqual(chats.shown?.key, Self.key, "another chat's rename")
        chats.renamed(Self.key, to: renamed)

        XCTAssertEqual(chats.shown?.key, renamed)
        XCTAssertEqual(chats.recording, renamed)
    }
}
