import XCTest

@testable import Aria

@MainActor
final class AssistantModelsTests: XCTestCase {
    private func model(_ id: String) -> AssistantModel {
        AssistantModel(id: id, name: id)
    }

    func testChangingAssistantDiscardsTheOlderModelResponse() async {
        let started = expectation(description: "first list started")
        var finishFirst: CheckedContinuation<[AssistantModel], Never>?
        let codex = model("codex-choice")
        let models = AssistantModels { assistantID in
            if assistantID == .claudeCode {
                return await withCheckedContinuation {
                    finishFirst = $0
                    started.fulfill()
                }
            }
            return [codex]
        }
        let first = Task { await models.load(.claudeCode) }
        await fulfillment(of: [started], timeout: 1)
        await models.load(.codex)
        finishFirst?.resume(returning: [model("stale-claude-choice")])
        await first.value

        XCTAssertEqual(models.choices.map(\.id), ["codex-choice"])
        XCTAssertFalse(models.isLoading)
        XCTAssertNil(models.failure)
    }

    func testFailedListingShowsANoticeAndTheNextLoadCanRecover() async {
        struct Failure: Error {}
        @MainActor final class ResponseState {
            var shouldFail = true
        }
        let state = ResponseState()
        let choice = model("choice")
        let models = AssistantModels { _ in
            if state.shouldFail { throw Failure() }
            return [choice]
        }
        await models.load(.codex)
        XCTAssertNotNil(models.failure)
        XCTAssertFalse(models.isLoading)
        state.shouldFail = false
        await models.load(.codex)
        XCTAssertNil(models.failure)
        XCTAssertEqual(models.choices.map(\.id), ["choice"])
    }
}
