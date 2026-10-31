import XCTest

@testable import Aria

final class AssistantEditsTests: XCTestCase {
    func testModelEditsPreserveTheOtherAssistantAndDefaultClearsTheChoice() {
        var snapshot = Aria_Daemon_V1_Settings()
        snapshot.assistantID = .claudeCode
        var pending = Aria_Daemon_V1_SettingsPatch()
        AssistantEdits.setModelID("claude-choice", for: .claudeCode, in: &pending)
        AssistantEdits.setModelID("codex-choice", for: .codex, in: &pending)
        pending.apply(to: &snapshot)
        XCTAssertEqual(AssistantEdits.modelID(in: snapshot), "claude-choice")
        snapshot.assistantID = .codex
        XCTAssertEqual(AssistantEdits.modelID(in: snapshot), "codex-choice")
        var reset = Aria_Daemon_V1_SettingsPatch()
        AssistantEdits.setModelID("", for: .codex, in: &reset)
        reset.apply(to: &snapshot)
        XCTAssertEqual(AssistantEdits.modelID(in: snapshot), "")
        snapshot.assistantID = .claudeCode
        XCTAssertEqual(AssistantEdits.modelID(in: snapshot), "claude-choice")
        reset.apply(to: &pending)
        XCTAssertEqual(pending.assistantModels.count, 2)
        XCTAssertEqual(pending.assistantModels.first { $0.assistantID == .codex }?.modelID, "")
        XCTAssertEqual(pending.assistantModels.first { $0.assistantID == .claudeCode }?.modelID, "claude-choice")
        AssistantEdits.setModelID("codex-later", for: .codex, in: &pending)
        XCTAssertEqual(pending.assistantModels.count, 2)
        XCTAssertEqual(pending.assistantModels.first { $0.assistantID == .codex }?.modelID, "codex-later")
    }

    func testModelIDsRejectOptionsWhitespaceAndOversizedValues() {
        for modelID in ["", "provider/model-1.2:latest", "opus", "gpt-example"] {
            XCTAssertTrue(AssistantEdits.isValidModelID(modelID))
        }
        for modelID in ["--help", "model name", "model\n", String(repeating: "a", count: 201)] {
            XCTAssertFalse(AssistantEdits.isValidModelID(modelID))
        }
    }
}
