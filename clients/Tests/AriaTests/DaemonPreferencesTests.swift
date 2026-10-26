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

    func testModelResetSurvivesConflictWithoutOverwritingTheOtherAssistant() async {
        var models = Aria_Daemon_V1_PreferencePatch()
        AssistantEdits.setModelID("claude-choice", for: .claudeCode, in: &models)
        AssistantEdits.setModelID("codex-choice", for: .codex, in: &models)
        var initial = snapshot(revision: 3)
        initial.preferences.assistantID = .codex
        models.apply(to: &initial.preferences)
        var changed = initial
        changed.revision = 4
        AssistantEdits.setModelID("claude-updated", for: .claudeCode, in: &models)
        models.apply(to: &changed.preferences)
        var saved = changed
        saved.revision = 5
        AssistantEdits.setModelID("", for: .codex, in: &models)
        models.apply(to: &saved.preferences)
        let transport = FakePreferencesTransport()
        transport.snapshots = [initial]
        transport.updates = [conflict(changed), accepted(saved)]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = {
            XCTAssertEqual(preferences.snapshot.flatMap { AssistantEdits.modelID(in: $0) }, "")
        }

        await preferences.set { AssistantEdits.setModelID("", for: .codex, in: &$0) }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertEqual(transport.sentUpdates.map { $0.patch.assistantModels.count }, [1, 1])
        for update in transport.sentUpdates {
            XCTAssertEqual(update.patch.assistantModels.first?.assistantID, .codex)
            XCTAssertEqual(update.patch.assistantModels.first?.modelID, "")
        }
        XCTAssertEqual(preferences.snapshot.flatMap { AssistantEdits.modelID(in: $0) }, "")
        XCTAssertEqual(
            preferences.snapshot?.assistantModels.first { $0.assistantID == .claudeCode }?.modelID, "claude-updated")
        XCTAssertNil(preferences.lastError)
    }

    func testAssistantEditStaysVisibleAndSurvivesARevisionConflict() async {
        var initial = snapshot(revision: 3)
        initial.preferences.assistantID = .claudeCode
        var changedElsewhere = snapshot(revision: 4, language: "auto")
        changedElsewhere.preferences.assistantID = .claudeCode
        var saved = snapshot(revision: 5, language: "auto")
        saved.preferences.assistantID = .codex
        let transport = FakePreferencesTransport()
        transport.snapshots = [initial]
        transport.updates = [conflict(changedElsewhere), accepted(saved)]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        transport.duringUpdate = {
            XCTAssertEqual(preferences.snapshot?.assistantID, .codex)
        }

        await preferences.set { $0.assistantID = .codex }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertEqual(transport.sentUpdates.map(\.patch.assistantID), [.codex, .codex])
        XCTAssertEqual(preferences.snapshot?.assistantID, .codex)
        XCTAssertEqual(preferences.snapshot?.recognitionLanguage, "auto")
        XCTAssertNil(preferences.lastError)
    }

    func testSyncUsesHistoryBooleanWithoutPresenceFallback() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 3)]
        let preferences = DaemonPreferences(transport: transport)
        XCTAssertTrue(preferences.isHistoryEnabled)

        await preferences.sync()

        XCTAssertFalse(preferences.isHistoryEnabled)
    }

    func testSetPushesTheKnownRevisionAndKeepsTheEditVisibleDuringSync() async {
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

    func testSyncAdoptsTheSnapshotAndIgnoresOlderRevisions() async {
        let transport = FakePreferencesTransport()
        transport.snapshots = [snapshot(revision: 5, mute: true), snapshot(revision: 4, mute: false)]
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()
        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
        XCTAssertNil(preferences.lastError)

        await preferences.sync()

        XCTAssertEqual(preferences.snapshot?.muteOutput, true)
    }

    func testRepeatedConflictsStopAndKeepEditsForTheNextSync() async {
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

    func testConflictRetriesTheLatestEdit() async {
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

    func testAcceptedFieldsAreNotReplayedWithLaterEdits() async {
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

    func testRefusalPreservesEditsDuringTheRequestAndRecovery() async {
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

    func testFailedRequestRestoresOlderFieldsUnderNewerValues() async {
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
                    $0.askHotkey = Aria_Daemon_V1_Hotkey()
                    $0.vocabularyDirectory = ""
                }
            }

            await preferences.set {
                $0.muteOutput = true
                $0.askHotkey.chord.keyCode = 7
                $0.askHotkey.chord.command = true
                $0.vocabularyDirectory = "/repo"
            }

            if missingResult {
                XCTAssertEqual(preferences.lastError as? DaemonPreferences.UpdateError, .invalidResponse)
            } else {
                XCTAssertTrue(preferences.lastError is FakePreferencesTransport.Unreachable)
            }
            XCTAssertEqual(preferences.snapshot?.muteOutput, true, "the edit stays visible")
            XCTAssertEqual(preferences.snapshot?.askHotkey.hasChord, false)
            XCTAssertEqual(preferences.snapshot?.hasVocabularyDirectory, false)
            transport.updateError = nil
            transport.updates = [accepted(snapshot(revision: 4, mute: true))]
            await preferences.sync()

            XCTAssertEqual(transport.sentUpdates.count, 2)
            let retried = transport.sentUpdates.last?.patch
            XCTAssertEqual(retried?.muteOutput, true)
            XCTAssertEqual(retried?.hasAskHotkey, true)
            XCTAssertEqual(retried?.askHotkey, Aria_Daemon_V1_Hotkey())
            XCTAssertEqual(retried?.hasVocabularyDirectory, true)
            XCTAssertEqual(retried?.vocabularyDirectory, "")
            XCTAssertNil(preferences.lastError)
        }
    }

    func testCancellationStopsRetriesAndKeepsTheEdit() async {
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

    func testConflictBudgetDoesNotRestartAfterAnAcceptedBatch() async {
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
