import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class DaemonPreferencesTests: XCTestCase {
    /// Answers from scripted queues; records every update it was asked for.
    final class FakeTransport: PreferencesTransport {
        var snapshots: [Aria_Daemon_V1_PreferenceSnapshot] = []
        var updates: [Aria_Daemon_V1_UpdatePreferencesResponse] = []
        var updateError: (any Error)?
        private(set) var sentUpdates: [Aria_Daemon_V1_UpdatePreferencesRequest] = []

        func snapshot() async throws -> Aria_Daemon_V1_PreferenceSnapshot {
            snapshots.removeFirst()
        }

        func update(
            _ request: Aria_Daemon_V1_UpdatePreferencesRequest
        ) async throws -> Aria_Daemon_V1_UpdatePreferencesResponse {
            sentUpdates.append(request)
            if let updateError { throw updateError }
            return updates.removeFirst()
        }
    }

    struct Unreachable: Error {}

    private func snapshot(
        revision: UInt64, mute: Bool = false, language: String = "en"
    ) -> Aria_Daemon_V1_PreferenceSnapshot {
        var snapshot = Aria_Daemon_V1_PreferenceSnapshot()
        snapshot.revision = revision
        snapshot.preferences.muteOutput = mute
        snapshot.preferences.recognitionLanguage = language
        return snapshot
    }

    private func accepted(_ snapshot: Aria_Daemon_V1_PreferenceSnapshot) -> Aria_Daemon_V1_UpdatePreferencesResponse {
        var response = Aria_Daemon_V1_UpdatePreferencesResponse()
        response.snapshot = snapshot
        return response
    }

    private func conflict(_ snapshot: Aria_Daemon_V1_PreferenceSnapshot) -> Aria_Daemon_V1_UpdatePreferencesResponse {
        var response = Aria_Daemon_V1_UpdatePreferencesResponse()
        response.conflict.current = snapshot
        return response
    }

    func test_sync_adoptsTheDaemonSnapshot() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3, mute: true)]
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()

        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        XCTAssertNil(preferences.lastError)
    }

    func test_sync_usesHistoryBooleanWithoutPresenceFallback() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3)]
        let preferences = DaemonPreferences(transport: transport)
        XCTAssertTrue(preferences.historyEnabled)

        await preferences.sync()

        XCTAssertFalse(preferences.historyEnabled)
    }

    func test_set_showsTheEditAtOnceAndPushesTheKnownRevision() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [accepted(snapshot(revision: 4, mute: true))]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.count, 1)
        XCTAssertEqual(transport.sentUpdates[0].expectedRevision, 3)
        XCTAssertEqual(transport.sentUpdates[0].patch.muteOutput, true)
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
    }

    func test_conflict_adoptsTheDaemonRevisionAndRetriesOnce() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            conflict(snapshot(revision: 7, language: "auto")),
            accepted(snapshot(revision: 8, mute: true, language: "auto")),
        ]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 7])
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
    }

    func test_sync_ignoresASnapshotOlderThanTheOneAlreadyKnown() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 5, mute: true), snapshot(revision: 4, mute: false)]
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()
        await preferences.sync()

        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
    }

    func test_refusedPatch_isDroppedAndTheDaemonTruthReturns() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
        transport.updateError = RPCError(code: .permissionDenied, message: "mac only")
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()

        await preferences.set { $0.muteOutput = true }
        XCTAssertEqual(preferences.snapshot?.muteOutput, false, "the refused edit is undone")

        // A later, acceptable edit is not dragged down by the refused one.
        transport.updateError = nil
        transport.updates = [accepted(snapshot(revision: 4, language: "auto"))]
        await preferences.set { $0.recognitionLanguage = "auto" }
        XCTAssertEqual(transport.sentUpdates.last?.patch.hasMuteOutput, false)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
    }

    func test_transportFailure_keepsThePendingEditAndReportsTheError() async {
        let transport = FakeTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
        transport.updateError = Unreachable()
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()

        await preferences.set { $0.muteOutput = true }
        XCTAssertTrue(preferences.lastError is Unreachable)
        XCTAssertEqual(preferences.snapshot?.muteOutput, true, "the edit stays visible")

        // The daemon comes back: the next sync pushes what was owed.
        transport.updateError = nil
        transport.updates = [accepted(snapshot(revision: 4, mute: true))]
        await preferences.sync()

        XCTAssertEqual(transport.sentUpdates.count, 2)
        XCTAssertNil(preferences.lastError)
    }
}
