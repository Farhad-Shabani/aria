import XCTest

@testable import Aria

@MainActor
final class AssistantSettingsTests: XCTestCase {
    func testAnAssistantThatDoesntAnswerAlwaysSaysWhy() {
        for status in Assistant.Status.allCases where !status.answers {
            let assistant = Assistant(id: .localModel, name: "Local Provider", status: status)
            XCTAssertNotNil(assistant.problem, "\(status)")
        }
    }

    func testOnlyAnAssistantWithoutADefaultAsksForAModel() {
        XCTAssertEqual(AssistantModelSection.noModelChoice(needsModel: false, modelID: ""), "Default")
        XCTAssertEqual(AssistantModelSection.noModelChoice(needsModel: false, modelID: "gpt"), "Default")
        XCTAssertEqual(AssistantModelSection.noModelChoice(needsModel: true, modelID: ""), "Choose a model")
        XCTAssertNil(AssistantModelSection.noModelChoice(needsModel: true, modelID: "qwen"))
    }
}
