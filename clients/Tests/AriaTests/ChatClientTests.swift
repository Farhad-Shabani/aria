import XCTest

@testable import Aria

final class ChatClientTests: XCTestCase {
    private func assistant(
        _ id: Aria_Daemon_V1_AssistantId, name: String, status: Aria_Daemon_V1_AssistantStatus = .ready
    ) -> Aria_Daemon_V1_Assistant {
        var assistant = Aria_Daemon_V1_Assistant()
        assistant.id = id
        assistant.name = name
        assistant.status = status
        return assistant
    }

    func testAssistantsKeepTheDaemonsOrderAndFix() throws {
        var codex = assistant(.codex, name: "Codex", status: .notInstalled)
        codex.fix = "install it"
        let assistants = try ChatClient.assistants(from: [codex, assistant(.claudeCode, name: "Claude")])
        XCTAssertEqual(
            assistants,
            [
                Assistant(id: .codex, name: "Codex", status: .notInstalled, fix: "install it"),
                Assistant(id: .claudeCode, name: "Claude", status: .ready),
            ])
    }

    func testTheLocalAssistantsStatusesComeThrough() throws {
        let statuses: [(Aria_Daemon_V1_AssistantStatus, Assistant.Status)] = [
            (.notConfigured, .notConfigured), (.unreachable, .unreachable), (.modelMissing, .modelMissing),
            (.modelUnset, .modelUnset),
        ]
        for (wire, status) in statuses {
            var local = assistant(.localModel, name: "Local Provider", status: wire)
            local.needsModel = true
            let assistants = try ChatClient.assistants(from: [local])
            XCTAssertEqual(
                assistants, [Assistant(id: .localModel, name: "Local Provider", status: status, needsModel: true)])
        }
    }

    func testAnEmptyDuplicateUnknownUnnamedOrStatuslessListIsInvalid() {
        let codex = assistant(.codex, name: "Codex")
        for wire in [
            [], [codex, codex], [assistant(.unspecified, name: "Unknown")],
            [assistant(.UNRECOGNIZED(99), name: "Unknown")], [assistant(.codex, name: " ")],
            [assistant(.codex, name: "Codex", status: .unspecified)],
            [assistant(.codex, name: String(repeating: "x", count: 201))],
            [assistant(.codex, name: "Co\u{07}dex")],
        ] {
            XCTAssertThrowsError(try ChatClient.assistants(from: wire)) {
                XCTAssertEqual($0 as? DaemonFailure, .invalidResponse)
            }
        }
    }

    func testTextFitsDeliveryUpToTheDaemonsLimitInBytes() {
        let limit = 64 * 1024
        XCTAssertTrue(ChatClient.fitsDelivery(String(repeating: "a", count: limit)))
        XCTAssertFalse(ChatClient.fitsDelivery(String(repeating: "a", count: limit + 1)))
        XCTAssertFalse(ChatClient.fitsDelivery(String(repeating: "é", count: limit / 2 + 1)))
    }

    func testAPromptRunOnAnEntryNamesItAndNoAppOrOutputMode() {
        let request = ChatClient.replyRequest(entry: 7, source: .stored(id: "summarize"))

        XCTAssertEqual(request.historyEntryID, 7)
        XCTAssertEqual(request.promptID, "summarize")
        XCTAssertFalse(request.hasOutputMode)
        XCTAssertTrue(request.applicationID.isEmpty)
    }

    func testAOneOffRunSendsItsInstructionsInPlaceOfAPrompt() {
        let request = ChatClient.replyRequest(entry: 7, source: .oneOff(instructions: "List the action items."))

        XCTAssertEqual(request.prompt, .instructions("List the action items."))
    }

    func testANewChatAsksTheDaemonToNameItInTheFolderInFront() {
        let request = ChatClient.startRequest(
            text: "When is lunch?", at: ChatTarget(workdir: "/code/aria", assistantID: .codex), ownVoice: true)

        XCTAssertEqual(request.text, "When is lunch?")
        XCTAssertEqual(request.assistantID, .codex)
        XCTAssertEqual(request.chat, .start(.with { $0.workdir = "/code/aria" }))
        XCTAssertTrue(request.ownVoice)
        XCTAssertTrue(request.tapCancels)
        XCTAssertFalse(request.hasOutputMode, "nothing is typed into the app in front")
    }

    func testANewChatTakesItsKeyFromTheReply() throws {
        let response = Aria_Daemon_V1_AskResponse.with {
            $0.ticket = 7
            $0.chat = .with {
                $0.assistantID = .codex
                $0.name = "c1"
            }
        }

        XCTAssertEqual(
            try ChatClient.started(response),
            StartedChat(ticket: 7, chat: ChatKey(assistantID: .codex, name: "c1")))
    }

    func testAReplyThatNamesNoChatIsInvalid() {
        let unnamed = Aria_Daemon_V1_AskResponse.with {
            $0.ticket = 7
            $0.chat = .with { $0.assistantID = .codex }
        }
        for response in [Aria_Daemon_V1_AskResponse.with { $0.ticket = 7 }, unnamed] {
            XCTAssertThrowsError(try ChatClient.started(response)) {
                XCTAssertEqual($0 as? DaemonFailure, .invalidResponse)
            }
        }
    }

    func testAFollowUpSendsItsChatAndKnownFolderButNoOutputMode() {
        let key = ChatKey(assistantID: .codex, name: "c1")
        let request = ChatClient.askRequest(
            text: "When is lunch?", in: key, folder: "/code/aria", ownVoice: true, tapCancels: false)

        XCTAssertEqual(request.text, "When is lunch?")
        XCTAssertEqual(request.assistantID, .codex)
        XCTAssertEqual(
            request.chat,
            .named(
                .with {
                    $0.name = "c1"
                    $0.workdir = "/code/aria"
                }),
            "the folder seeds a chat whose folder record the daemon lost")
        XCTAssertEqual(
            ChatClient.askRequest(text: "When is lunch?", in: key, folder: nil, ownVoice: true, tapCancels: false)
                .chat,
            .named(.with { $0.name = "c1" }), "an unknown folder seeds nothing")
        XCTAssertTrue(request.ownVoice)
        XCTAssertFalse(request.tapCancels)
        XCTAssertFalse(request.hasOutputMode, "nothing is typed into the app in front")
    }
}
