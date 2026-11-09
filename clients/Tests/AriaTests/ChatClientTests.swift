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
        ]
        for (wire, status) in statuses {
            let assistants = try ChatClient.assistants(from: [assistant(.localModel, name: "Local model", status: wire)]
            )
            XCTAssertEqual(assistants, [Assistant(id: .localModel, name: "Local model", status: status)])
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
}
