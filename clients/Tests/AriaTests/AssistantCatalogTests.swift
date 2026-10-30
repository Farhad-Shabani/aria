import XCTest

@testable import Aria

@MainActor
final class AssistantCatalogTests: XCTestCase {
    private func assistant(
        _ id: Aria_Daemon_V1_AssistantID, name: String, status: Aria_Daemon_V1_AssistantStatus = .ready
    ) -> Aria_Daemon_V1_Assistant {
        var assistant = Aria_Daemon_V1_Assistant()
        assistant.id = id
        assistant.name = name
        assistant.status = status
        return assistant
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

    func testLoadUnlessReadyRetriesUntilEveryAssistantIsReady() async {
        struct Failure: Error {}
        @MainActor final class ResponseState {
            var shouldFail = true
            var status = Aria_Daemon_V1_AssistantStatus.signedOut
            var calls = 0
        }
        let state = ResponseState()
        let catalog = AssistantCatalog {
            state.calls += 1
            if state.shouldFail { throw Failure() }
            return [self.assistant(.codex, name: "Codex", status: state.status)]
        }
        await catalog.loadUnlessReady()
        XCTAssertNotNil(catalog.failure)
        state.shouldFail = false
        await catalog.loadUnlessReady()
        XCTAssertNil(catalog.failure)
        state.status = .ready
        await catalog.loadUnlessReady()
        await catalog.loadUnlessReady()
        XCTAssertEqual(state.calls, 3)
    }

    func testCatalogRejectsEmptyDuplicateUnknownUnnamedAndStatuslessChoices() async {
        let codex = assistant(.codex, name: "Codex")
        for choices in [
            [], [codex, codex], [assistant(.unspecified, name: "Unknown")],
            [assistant(.UNRECOGNIZED(99), name: "Unknown")], [assistant(.codex, name: " ")],
            [assistant(.codex, name: "Codex", status: .unspecified)],
        ] {
            let catalog = AssistantCatalog { choices }
            await catalog.load()
            XCTAssertTrue(catalog.choices.isEmpty)
            XCTAssertNotNil(catalog.failure)
        }
    }

    func testOlderResponseCannotReplaceTheLatestCatalog() async {
        let started = expectation(description: "first catalog started")
        var finishFirst: CheckedContinuation<[Aria_Daemon_V1_Assistant], Never>?
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
