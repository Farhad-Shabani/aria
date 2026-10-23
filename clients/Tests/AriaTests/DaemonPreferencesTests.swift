import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class DaemonPreferencesTests: XCTestCase {
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

    func test_sync_usesHistoryBooleanWithoutPresenceFallback() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3)]
        let preferences = DaemonPreferences(transport: transport)
        XCTAssertTrue(preferences.historyEnabled)

        await preferences.sync()

        XCTAssertFalse(preferences.historyEnabled)
    }

    func test_set_pushesTheKnownRevisionAndKeepsTheEditVisibleDuringSync() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
        transport.updates = [accepted(snapshot(revision: 4, mute: true))]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = {
            await preferences.sync()
            XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        }

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.count, 1)
        XCTAssertEqual(transport.sentUpdates[0].expectedRevision, 3)
        XCTAssertEqual(transport.sentUpdates[0].patch.muteOutput, true)
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        XCTAssertNil(preferences.lastError)
    }

    func test_conflict_adoptsTheDaemonRevisionAndRetriesOnce() async {
        let transport = FakePreferencesTransport()
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

    func test_sync_adoptsTheSnapshotAndIgnoresOlderRevisions() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 5, mute: true), snapshot(revision: 4, mute: false)]
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        XCTAssertNil(preferences.lastError)

        await preferences.sync()

        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
    }

    func test_repeatedConflicts_stopAndKeepEditsForTheNextSync() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 8, language: "auto")]
        transport.updates = [
            conflict(snapshot(revision: 7)),
            conflict(snapshot(revision: 8, language: "auto")),
            accepted(snapshot(revision: 9, mute: true, language: "auto")),
        ]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 7])
        XCTAssertNotNil(preferences.lastError)
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")

        await preferences.sync()

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 7, 8])
        XCTAssertNil(preferences.lastError)
    }

    func test_conflict_retriesTheLatestEdit() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [conflict(snapshot(revision: 4)), accepted(snapshot(revision: 5))]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = { await preferences.set { $0.muteOutput = false } }

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.map(\.patch.muteOutput), [true, false])
        XCTAssertEqual(preferences.snapshot?.muteOutput, false)
        XCTAssertNil(preferences.lastError)
    }

    func test_acceptedFields_areNotReplayedWithLaterEdits() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            accepted(snapshot(revision: 4, mute: true)),
            accepted(snapshot(revision: 5, mute: true, language: "auto")),
        ]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = { await preferences.set { $0.recognitionLanguage = "auto" } }

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.count, 2)
        XCTAssertEqual(transport.sentUpdates.last?.patch.hasMuteOutput, false)
        XCTAssertEqual(transport.sentUpdates.last?.patch.recognitionLanguage, "auto")
    }

    func test_refusal_preservesEditsDuringTheRequestAndRecovery() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3), snapshot(revision: 3)]
        transport.updateError = RPCError(code: .permissionDenied, message: "mac only")
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = { await preferences.set { $0.recognitionLanguage = "auto" } }
        transport.duringSnapshot = { await preferences.set { $0.dictationHistory = false } }

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(preferences.snapshot?.muteOutput, false)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
        XCTAssertNotNil(preferences.lastError)
        transport.updateError = nil
        transport.updates = [accepted(snapshot(revision: 4, language: "auto"))]
        await preferences.sync()

        XCTAssertEqual(transport.sentUpdates.last?.patch.hasMuteOutput, false)
        XCTAssertEqual(transport.sentUpdates.last?.patch.recognitionLanguage, "auto")
        XCTAssertEqual(transport.sentUpdates.last?.patch.hasDictationHistory, true)
        XCTAssertEqual(transport.sentUpdates.last?.patch.dictationHistory, false)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
    }

    func test_failedRequest_restoresOlderFieldsUnderNewerValues() async {
        for missingResult in [false, true] {
            let transport = FakePreferencesTransport()
            transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
            if missingResult {
                transport.updates = [Aria_Daemon_V1_UpdatePreferencesResponse()]
            } else {
                transport.updateError = FakePreferencesTransport.Unreachable()
            }
            let preferences = DaemonPreferences(transport: transport)
            await preferences.sync()
            transport.duringUpdate = {
                await preferences.set {
                    $0.askHotkey = Aria_Daemon_V1_AskHotkey()
                    $0.vocabularyDirectory = ""
                }
            }

            await preferences.set {
                $0.muteOutput = true
                $0.askHotkey.enabled = true
                $0.askHotkey.keyCode = 7
                $0.askHotkey.command = true
                $0.vocabularyDirectory = "/repo"
            }

            if missingResult {
                XCTAssertEqual(preferences.lastError as? DaemonPreferences.UpdateError, .invalidResponse)
            } else {
                XCTAssertTrue(preferences.lastError is FakePreferencesTransport.Unreachable)
            }
            XCTAssertEqual(preferences.snapshot?.muteOutput, true, "the edit stays visible")
            XCTAssertEqual(preferences.snapshot?.askHotkey.enabled, false)
            XCTAssertEqual(preferences.snapshot?.hasVocabularyDirectory, false)
            transport.updateError = nil
            transport.updates = [accepted(snapshot(revision: 4, mute: true))]
            await preferences.sync()

            XCTAssertEqual(transport.sentUpdates.count, 2)
            let retried = transport.sentUpdates.last?.patch
            XCTAssertEqual(retried?.muteOutput, true)
            XCTAssertEqual(retried?.hasAskHotkey, true)
            XCTAssertEqual(retried?.askHotkey, Aria_Daemon_V1_AskHotkey())
            XCTAssertEqual(retried?.hasVocabularyDirectory, true)
            XCTAssertEqual(retried?.vocabularyDirectory, "")
            XCTAssertNil(preferences.lastError)
        }
    }

    func test_cancellation_stopsRetriesAndKeepsTheEdit() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 4)]
        transport.updates = [conflict(snapshot(revision: 4)), accepted(snapshot(revision: 5, mute: true))]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = { withUnsafeCurrentTask { $0?.cancel() } }

        await Task { await preferences.set { $0.muteOutput = true } }.value

        XCTAssertEqual(transport.sentUpdates.count, 1)
        XCTAssertTrue(preferences.lastError is CancellationError)
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        await preferences.sync()
        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertNil(preferences.lastError)
    }

    func test_conflictBudget_doesNotRestartAfterAnAcceptedBatch() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            conflict(snapshot(revision: 4)),
            accepted(snapshot(revision: 5, mute: true)),
            conflict(snapshot(revision: 6, mute: true)),
            accepted(snapshot(revision: 7, mute: true, language: "auto")),
        ]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = {
            transport.duringUpdate = { await preferences.set { $0.recognitionLanguage = "auto" } }
        }

        await preferences.set { $0.muteOutput = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4, 5])
        XCTAssertEqual(preferences.lastError as? DaemonPreferences.UpdateError, .conflict)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
    }

}
