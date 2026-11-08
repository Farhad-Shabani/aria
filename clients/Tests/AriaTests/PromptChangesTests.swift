import XCTest

@testable import Aria

@MainActor
final class PromptChangesTests: XCTestCase {
    func testAPromptAnotherClientRemovedCountsAsRemoved() async {
        var refreshes = 0
        let changes = PromptChanges(
            drop: { _ in throw DaemonFailure.refused(.promptMissing) }, refresh: { refreshes += 1 })

        let failure = await changes.remove("gone")

        XCTAssertNil(failure)
        XCTAssertEqual(refreshes, 1)
    }

    func testAFailedChangeSaysWhyAndRefreshesNothing() async {
        var refreshes = 0
        let changes = PromptChanges(
            send: { _ in throw DaemonFailure.refused(.promptsFull) },
            drop: { _ in throw DaemonFailure.unavailable },
            refresh: { refreshes += 1 })

        let saved = await changes.save(Aria_Daemon_V1_SavePromptRequest())
        let removed = await changes.remove("summary")

        XCTAssertEqual(saved, .failure(.full))
        XCTAssertEqual(removed, .offline)
        XCTAssertEqual(refreshes, 0)
    }
}
