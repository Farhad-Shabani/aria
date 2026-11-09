import XCTest

@testable import Aria

@MainActor
final class AssistantCatalogTests: XCTestCase {
    private func assistant(
        _ id: Aria_Daemon_V1_AssistantId, name: String, status: Assistant.Status = .ready
    ) -> Assistant {
        Assistant(id: id, name: name, status: status)
    }

    func testCatalogUsesDaemonOrderAndNamesAndRecoversAfterFailure() async {
        struct Failure: Error {}
        let choices = [assistant(.codex, name: "Server Codex"), assistant(.claudeCode, name: "Server Claude")]
        @MainActor final class ResponseState {
            var shouldFail = true
        }
        let state = ResponseState()
        let catalog = AssistantCatalog {
            if state.shouldFail { throw Failure() }
            return choices
        }
        await catalog.load()
        XCTAssertNotNil(catalog.failure)
        XCTAssertFalse(catalog.isLoading)
        state.shouldFail = false
        await catalog.load()
        XCTAssertEqual(catalog.choices, choices)
        XCTAssertNil(catalog.failure)
        XCTAssertFalse(catalog.isLoading)
    }

    func testLoadUnlessReadyRechecksOnlyTheChosenAssistantUntilNothingOutsideAriaIsLeft() async {
        struct Failure: Error {}
        @MainActor final class ResponseState {
            var shouldFail = true
            var local = Assistant.Status.unreachable
            var calls = 0
        }
        let state = ResponseState()
        let catalog = AssistantCatalog {
            state.calls += 1
            if state.shouldFail { throw Failure() }
            return [
                self.assistant(.localModel, name: "Local Provider", status: state.local),
                self.assistant(.claudeCode, name: "Claude Code", status: .notInstalled),
            ]
        }
        await catalog.loadUnlessReady(selected: .localModel)
        XCTAssertNotNil(catalog.failure)
        state.shouldFail = false
        await catalog.loadUnlessReady(selected: .localModel)
        XCTAssertNil(catalog.failure)
        state.local = .modelUnset
        await catalog.loadUnlessReady(selected: .localModel)
        await catalog.loadUnlessReady(selected: .localModel)
        XCTAssertEqual(state.calls, 3)
        await catalog.loadUnlessReady(selected: .claudeCode)
        XCTAssertEqual(state.calls, 4)
    }

    func testOlderResponseCannotReplaceTheLatestCatalog() async {
        let started = expectation(description: "first catalog started")
        var finishFirst: CheckedContinuation<[Assistant], Never>?
        let latest = [assistant(.codex, name: "Latest")]
        var calls = 0
        let catalog = AssistantCatalog {
            calls += 1
            if calls == 1 {
                return await withCheckedContinuation {
                    finishFirst = $0
                    started.fulfill()
                }
            }
            return latest
        }
        let first = Task { await catalog.load() }
        await fulfillment(of: [started], timeout: 1)
        await catalog.load()
        finishFirst?.resume(returning: [assistant(.claudeCode, name: "Older")])
        await first.value
        XCTAssertEqual(catalog.choices, latest)
        XCTAssertNil(catalog.failure)
        XCTAssertFalse(catalog.isLoading)
    }

    func testCancelledLoadDoesNotPublishChoicesOrFailure() async {
        let catalog = AssistantCatalog { [self.assistant(.codex, name: "Codex")] }
        await Task {
            withUnsafeCurrentTask { $0?.cancel() }
            await catalog.load()
        }.value
        XCTAssertTrue(catalog.choices.isEmpty)
        XCTAssertNil(catalog.failure)
        XCTAssertFalse(catalog.isLoading)
    }
}
