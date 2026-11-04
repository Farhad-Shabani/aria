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
}
