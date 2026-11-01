import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class DaemonSettingsTests: XCTestCase {
    private func snapshot(
        revision: UInt64, mute: Bool = false, language: String = "en"
    ) -> Aria_Daemon_V1_SettingsSnapshot {
        var snapshot = Aria_Daemon_V1_SettingsSnapshot()
        snapshot.revision = revision
        snapshot.settings.muteSound = mute
        snapshot.settings.sttLanguage = language
        return snapshot
    }

    private func accepted(_ snapshot: Aria_Daemon_V1_SettingsSnapshot) -> Aria_Daemon_V1_UpdateSettingsResponse {
        var response = Aria_Daemon_V1_UpdateSettingsResponse()
        response.snapshot = snapshot
        return response
    }

    private func conflict(_ snapshot: Aria_Daemon_V1_SettingsSnapshot) -> Aria_Daemon_V1_UpdateSettingsResponse {
        var response = Aria_Daemon_V1_UpdateSettingsResponse()
        response.conflict.current = snapshot
        return response
    }

    func testModelResetSurvivesConflictWithoutOverwritingTheOtherAssistant() async {
        var models = Aria_Daemon_V1_SettingsPatch()
        AssistantEdits.setModelID("claude-choice", for: .claudeCode, in: &models)
        AssistantEdits.setModelID("codex-choice", for: .codex, in: &models)
        var initial = snapshot(revision: 3)
        initial.settings.assistantID = .codex
        models.apply(to: &initial.settings)
        var changed = initial
        changed.revision = 4
        AssistantEdits.setModelID("claude-updated", for: .claudeCode, in: &models)
        models.apply(to: &changed.settings)
        var saved = changed
        saved.revision = 5
        AssistantEdits.setModelID("", for: .codex, in: &models)
        models.apply(to: &saved.settings)
        let transport = FakeSettingsTransport()
        transport.snapshots = [initial]
        transport.updates = [conflict(changed), accepted(saved)]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            XCTAssertEqual(settings.snapshot.flatMap { AssistantEdits.modelID(in: $0) }, "")
        }

        await settings.set { AssistantEdits.setModelID("", for: .codex, in: &$0) }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertEqual(transport.sentUpdates.map { $0.patch.assistantModels.count }, [1, 1])
        for update in transport.sentUpdates {
            XCTAssertEqual(update.patch.assistantModels.first?.assistantID, .codex)
            XCTAssertEqual(update.patch.assistantModels.first?.modelID, "")
        }
        XCTAssertEqual(settings.snapshot.flatMap { AssistantEdits.modelID(in: $0) }, "")
        XCTAssertEqual(
            settings.snapshot?.assistantModels.first { $0.assistantID == .claudeCode }?.modelID, "claude-updated")
        XCTAssertNil(settings.lastError)
    }

    func testAssistantEditStaysVisibleAndSurvivesARevisionConflict() async {
        var initial = snapshot(revision: 3)
        initial.settings.assistantID = .claudeCode
        var changedElsewhere = snapshot(revision: 4, language: "auto")
        changedElsewhere.settings.assistantID = .claudeCode
        var saved = snapshot(revision: 5, language: "auto")
        saved.settings.assistantID = .codex
        let transport = FakeSettingsTransport()
        transport.snapshots = [initial]
        transport.updates = [conflict(changedElsewhere), accepted(saved)]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            XCTAssertEqual(settings.snapshot?.assistantID, .codex)
        }

        await settings.set { $0.assistantID = .codex }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertEqual(transport.sentUpdates.map(\.patch.assistantID), [.codex, .codex])
        XCTAssertEqual(settings.snapshot?.assistantID, .codex)
        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")
        XCTAssertNil(settings.lastError)
    }

    func testSyncUsesHistoryBooleanWithoutPresenceFallback() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        let settings = DaemonSettings(transport: transport)
        XCTAssertTrue(settings.isHistoryEnabled)

        await settings.sync()

        XCTAssertFalse(settings.isHistoryEnabled)
    }

    func testSetPushesTheKnownRevisionAndKeepsTheEditVisibleDuringSync() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
        transport.updates = [accepted(snapshot(revision: 4, mute: true))]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            await settings.sync()
            XCTAssertEqual(settings.snapshot?.muteSound, true)
        }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.count, 1)
        XCTAssertEqual(transport.sentUpdates[0].expectedRevision, 3)
        XCTAssertEqual(transport.sentUpdates[0].patch.muteSound, true)
        XCTAssertEqual(settings.snapshot?.muteSound, true)
        XCTAssertNil(settings.lastError)
    }

    func testASyncAdoptsAHandEditWhateverItsHash() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 5, mute: true), snapshot(revision: 4, mute: false)]
        let settings = DaemonSettings(transport: transport)

        await settings.sync()
        XCTAssertEqual(settings.snapshot?.muteSound, true)

        await settings.sync()

        XCTAssertEqual(settings.snapshot?.muteSound, false)
    }

    func testAFetchOverlappingAPushCannotRevertIt() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3, mute: false)]
        transport.updates = [accepted(snapshot(revision: 9, mute: true))]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringSnapshot = {
            await settings.set { $0.muteSound = true }
        }

        await settings.sync()

        XCTAssertEqual(settings.snapshot?.muteSound, true)
    }

    func testRepeatedConflictsStopAndKeepEditsForTheNextSync() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 8, language: "auto")]
        transport.updates = [
            conflict(snapshot(revision: 7)),
            conflict(snapshot(revision: 8, language: "auto")),
            accepted(snapshot(revision: 9, mute: true, language: "auto")),
        ]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 7])
        XCTAssertNotNil(settings.lastError)
        XCTAssertEqual(settings.snapshot?.muteSound, true)
        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")

        await settings.sync()

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 7, 8])
        XCTAssertNil(settings.lastError)
    }

    func testAConflictRetriesTheSameEditAndLaterOnesFollow() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            conflict(snapshot(revision: 4)),
            accepted(snapshot(revision: 5, mute: true)),
            accepted(snapshot(revision: 6, mute: false)),
        ]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = { await settings.set { $0.muteSound = false } }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4, 5])
        XCTAssertEqual(transport.sentUpdates.map(\.patch.muteSound), [true, true, false])
        XCTAssertEqual(settings.snapshot?.muteSound, false)
        XCTAssertNil(settings.lastError)
    }

    func testAConflictRetryKeepsAListEntryAddedElsewhere() async {
        var initial = snapshot(revision: 3)
        initial.settings.prompts.entries = [prompt("mac-old")]
        var changedElsewhere = snapshot(revision: 4)
        changedElsewhere.settings.prompts.entries = [prompt("mac-old"), prompt("phone")]
        var saved = snapshot(revision: 5)
        saved.settings.prompts.entries = [prompt("mac-old"), prompt("phone"), prompt("mac-new")]
        let transport = FakeSettingsTransport()
        transport.snapshots = [initial]
        transport.updates = [conflict(changedElsewhere), accepted(saved)]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()

        await settings.edit(promptsEdit { PromptEdits.add(self.prompt("mac-new"), to: &$0) })

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertEqual(
            transport.sentUpdates.map { $0.patch.prompts.entries.map(\.id) },
            [["mac-old", "mac-new"], ["mac-old", "phone", "mac-new"]])
        XCTAssertEqual(settings.snapshot?.prompts.entries.map(\.id), ["mac-old", "phone", "mac-new"])
        XCTAssertNil(settings.lastError)
    }

    func testAcceptedFieldsAreNotReplayedWithLaterEdits() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            accepted(snapshot(revision: 4, mute: true)),
            accepted(snapshot(revision: 5, mute: true, language: "auto")),
        ]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = { await settings.set { $0.sttLanguage = "auto" } }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.count, 2)
        XCTAssertEqual(transport.sentUpdates.last?.patch.hasMuteSound, false)
        XCTAssertEqual(transport.sentUpdates.last?.patch.sttLanguage, "auto")
    }

    func testARefusedEditDoesNotTakeTheOnesBehindItWithIt() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updateError = RPCError(code: .permissionDenied, message: "mac only")
        transport.updates = [accepted(snapshot(revision: 4, language: "auto"))]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            await settings.set { $0.sttLanguage = "auto" }
            // Only the first request is refused.
            transport.duringUpdate = { transport.updateError = nil }
        }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 3])
        XCTAssertEqual(transport.sentUpdates.map(\.patch.hasMuteSound), [true, false])
        XCTAssertEqual(transport.sentUpdates.last?.patch.sttLanguage, "auto")
        XCTAssertEqual(settings.snapshot?.muteSound, false, "the refused edit leaves the view")
        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")
        XCTAssertEqual((settings.lastError as? RPCError)?.code, .permissionDenied, "the refusal outlives later edits")
        XCTAssertFalse(settings.hasUnsavedChange)
    }

    func testAListEditQueuedBehindARefusedOneRebasesOnTheDaemonsList() async {
        var initial = snapshot(revision: 3)
        initial.settings.prompts.entries = [prompt("a"), prompt("b")]
        var saved = snapshot(revision: 4)
        saved.settings.prompts.entries = [prompt("a"), prompt("b", name: "B renamed")]
        let transport = FakeSettingsTransport()
        transport.snapshots = [initial]
        transport.updateError = RPCError(code: .invalidArgument, message: "shortcut in use")
        transport.updates = [accepted(saved)]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            await settings.edit(self.promptsEdit { PromptEdits.update("b", in: &$0) { $0.name = "B renamed" } })
            transport.duringUpdate = { transport.updateError = nil }
        }

        await settings.edit(promptsEdit { PromptEdits.update("a", in: &$0) { $0.chord.keyCode = 7 } })

        XCTAssertEqual(transport.sentUpdates.count, 2)
        let rename = transport.sentUpdates.last?.patch.prompts.entries
        XCTAssertEqual(rename?.first { $0.id == "a" }?.hasChord, false, "the refused chord is not carried forward")
        XCTAssertEqual(rename?.first { $0.id == "b" }?.name, "B renamed")
        XCTAssertEqual(settings.snapshot?.prompts.entries.first { $0.id == "a" }?.hasChord, false)
        XCTAssertEqual(settings.snapshot?.prompts.entries.first { $0.id == "b" }?.name, "B renamed")
        XCTAssertEqual((settings.lastError as? RPCError)?.code, .invalidArgument)
    }

    func testATransientFailureKeepsEveryEditInOrder() async {
        for missingResult in [false, true] {
            let transport = FakeSettingsTransport()
            transport.snapshots = [snapshot(revision: 3), snapshot(revision: 3)]
            if missingResult {
                transport.updates = [Aria_Daemon_V1_UpdateSettingsResponse()]
            } else {
                transport.updateError = FakeSettingsTransport.Unreachable()
            }
            let settings = DaemonSettings(transport: transport)
            await settings.sync()
            transport.duringUpdate = {
                await settings.set {
                    $0.askShortcut = Aria_Daemon_V1_Shortcut()
                    $0.vocabularyDir = ""
                }
            }

            await settings.set {
                $0.muteSound = true
                $0.askShortcut.chord.keyCode = 7
                $0.askShortcut.chord.command = true
                $0.vocabularyDir = "/repo"
            }

            if missingResult {
                XCTAssertEqual(settings.lastError as? DaemonSettings.UpdateError, .invalidResponse)
            } else {
                XCTAssertTrue(settings.lastError is FakeSettingsTransport.Unreachable)
            }
            XCTAssertTrue(settings.hasUnsavedChange)
            XCTAssertEqual(settings.snapshot?.muteSound, true, "both edits stay visible")
            XCTAssertEqual(settings.snapshot?.askShortcut, Aria_Daemon_V1_Shortcut())
            XCTAssertEqual(settings.snapshot?.hasVocabularyDir, false)
            transport.updateError = nil
            var first = snapshot(revision: 4, mute: true)
            first.settings.askShortcut.chord.keyCode = 7
            first.settings.askShortcut.chord.command = true
            first.settings.vocabularyDir = "/repo"
            transport.updates = [accepted(first), accepted(snapshot(revision: 5, mute: true))]
            await settings.sync()

            XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 3, 4])
            XCTAssertEqual(transport.sentUpdates[1].patch, transport.sentUpdates[0].patch, "the failed edit goes first")
            let second = transport.sentUpdates.last?.patch
            XCTAssertEqual(second?.hasMuteSound, false)
            XCTAssertEqual(second?.askShortcut, Aria_Daemon_V1_Shortcut())
            XCTAssertEqual(second?.vocabularyDir, "")
            XCTAssertNil(settings.lastError)
            XCTAssertFalse(settings.hasUnsavedChange)
        }
    }

    func testSyncWithAPendingEditStillShowsWhatChangedElsewhere() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 8, language: "auto")]
        transport.updateError = FakeSettingsTransport.Unreachable()
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        await settings.set { $0.muteSound = true }
        XCTAssertNotNil(settings.lastError)

        await settings.sync()

        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")
        XCTAssertEqual(settings.snapshot?.muteSound, true)
        XCTAssertTrue(settings.hasUnsavedChange)
    }

    func testCancellationKeepsTheEditWithoutAnError() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3), snapshot(revision: 4)]
        transport.updates = [conflict(snapshot(revision: 4)), accepted(snapshot(revision: 5, mute: true))]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = { withUnsafeCurrentTask { $0?.cancel() } }

        await Task { await settings.set { $0.muteSound = true } }.value

        XCTAssertEqual(transport.sentUpdates.count, 1)
        XCTAssertNil(settings.lastError)
        XCTAssertEqual(settings.snapshot?.muteSound, true)
        await settings.sync()
        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4])
        XCTAssertNil(settings.lastError)
    }

    func testTheConflictBudgetResetsWithEachAcceptedEdit() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            conflict(snapshot(revision: 4)),
            accepted(snapshot(revision: 5, mute: true)),
            conflict(snapshot(revision: 6, mute: true)),
            accepted(snapshot(revision: 7, mute: true, language: "auto")),
        ]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            transport.duringUpdate = { await settings.set { $0.sttLanguage = "auto" } }
        }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4, 5, 6])
        XCTAssertNil(settings.lastError)
        XCTAssertEqual(settings.snapshot?.muteSound, true)
        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")
    }

    func testARefusedEditLeavesNoConflictsForTheNextOne() async {
        let transport = FakeSettingsTransport()
        transport.snapshots = [snapshot(revision: 3)]
        transport.updates = [
            conflict(snapshot(revision: 4)),
            conflict(snapshot(revision: 5)),
            accepted(snapshot(revision: 6, language: "auto")),
        ]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        transport.duringUpdate = {
            await settings.set { $0.sttLanguage = "auto" }
            transport.duringUpdate = {
                transport.updateError = RPCError(code: .permissionDenied, message: "mac only")
                transport.duringUpdate = { transport.updateError = nil }
            }
        }

        await settings.set { $0.muteSound = true }

        XCTAssertEqual(transport.sentUpdates.map(\.expectedRevision), [3, 4, 4, 5])
        XCTAssertEqual(settings.snapshot?.sttLanguage, "auto")
        XCTAssertFalse(settings.hasUnsavedChange)
    }

    private func prompt(_ id: String, name: String? = nil) -> Aria_Daemon_V1_Prompt {
        var prompt = Aria_Daemon_V1_Prompt()
        prompt.id = id
        prompt.name = name ?? id
        prompt.kind = .rewrite
        prompt.file = "/prompts/\(id).md"
        prompt.enabled = true
        return prompt
    }

    /// A whole-list edit, as `SettingsEditor.editLatest` builds one.
    private func promptsEdit(_ change: @escaping (inout Aria_Daemon_V1_Prompts) -> Void) -> DaemonSettings.Edit {
        { base in
            var prompts = base.prompts
            change(&prompts)
            var patch = Aria_Daemon_V1_SettingsPatch()
            patch.prompts = prompts
            return patch
        }
    }
}
