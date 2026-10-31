import XCTest

@testable import Aria

final class DictationEditsTests: XCTestCase {
    func testCopyOutputForcesCompleteDelivery() {
        var patch = Aria_Daemon_V1_SettingsPatch()
        DictationEdits.setOutput(.copy, in: &patch)
        XCTAssertEqual(patch.outputMode, .copy)
        XCTAssertEqual(patch.deliveryMode, .complete)
    }

    func testInsertOutputLeavesDeliveryAlone() {
        var patch = Aria_Daemon_V1_SettingsPatch()
        DictationEdits.setOutput(.insert, in: &patch)
        XCTAssertEqual(patch.outputMode, .insert)
        XCTAssertFalse(patch.hasDeliveryMode)
    }

    func testSilenceMillisecondsRoundsToWholeMilliseconds() {
        XCTAssertEqual(DictationEdits.silenceMilliseconds(seconds: 1.5), 1500)
        XCTAssertEqual(DictationEdits.silenceMilliseconds(seconds: 0.2999999), 300)
    }

    func testLanguageChoicesAppendOnlyAnUnknownStoredCode() {
        XCTAssertEqual(DictationEdits.languageChoices(including: "en"), ["auto", "en"])
        XCTAssertEqual(DictationEdits.languageChoices(including: ""), ["auto", "en"])
        XCTAssertEqual(DictationEdits.languageChoices(including: "pt-br"), ["auto", "en", "pt-br"])
    }

    func testAutoStopChoicesListAnUnknownStoredValueInOrder() {
        XCTAssertEqual(DictationEdits.autoStopChoices(including: 10000), DictationEdits.autoStopChoices)
        XCTAssertEqual(
            DictationEdits.autoStopChoices(including: 7500),
            [0, 3000, 5000, 7500, 10000, 15000, 20000, 30000, 45000, 60000]
        )
    }

    func testAutoStopOffClearsTheSnapshot() {
        var snapshot = Aria_Daemon_V1_Settings()
        snapshot.autoStopMs = 10000
        var patch = Aria_Daemon_V1_SettingsPatch()
        patch.autoStopMs = 0
        patch.apply(to: &snapshot)
        XCTAssertFalse(snapshot.hasAutoStopMs)
    }

    /// The two messages share field numbers, so a protobuf merge is the
    /// reference the hand-written overlay must match field for field.
    func testPatchApplyCoversEveryField() throws {
        var patch = Aria_Daemon_V1_SettingsPatch()
        patch.sttLanguage = "de"
        patch.outputMode = .copy
        patch.deliveryMode = .complete
        patch.silenceDurationMs = 2500
        patch.autoStopMs = 15000
        patch.ownVoiceOnly = true
        patch.ariaKey = .fn
        patch.askShortcut.chord.keyCode = 7
        patch.promptListShortcut.chord.keyCode = 8
        patch.vocabularyEnabled = true
        patch.vocabularyDir = "/repo"
        patch.historyEnabled = false
        patch.historyKeepAudio = true
        patch.muteSound = true
        patch.prompts.entries = [PromptEdits.makeEntry(for: URL(fileURLWithPath: "/p.md"))]
        var combined = Aria_Daemon_V1_SettingsPatch()
        patch.apply(to: &combined)
        XCTAssertEqual(combined, patch)
        var overlaid = Aria_Daemon_V1_Settings()
        patch.apply(to: &overlaid)
        var merged = Aria_Daemon_V1_Settings()
        let bytes: [UInt8] = try patch.serializedBytes()
        try merged.merge(serializedBytes: bytes)
        XCTAssertEqual(overlaid, merged)
    }

    func testPatchApplyLaysOnlySetFieldsOverSnapshot() {
        var snapshot = Aria_Daemon_V1_Settings()
        snapshot.outputMode = .insert
        snapshot.muteSound = true
        snapshot.vocabularyDir = "/repo"
        var patch = Aria_Daemon_V1_SettingsPatch()
        patch.outputMode = .copy
        patch.vocabularyDir = ""
        patch.apply(to: &snapshot)
        XCTAssertEqual(snapshot.outputMode, .copy)
        XCTAssertTrue(snapshot.muteSound)
        XCTAssertFalse(snapshot.hasVocabularyDir)
    }

    func testPatchApplyReplacesEveryPrompt() {
        var snapshot = Aria_Daemon_V1_Settings()
        snapshot.prompts.entries = [
            PromptEdits.makeEntry(for: URL(fileURLWithPath: "/a.md")),
            PromptEdits.makeEntry(for: URL(fileURLWithPath: "/b.md")),
        ]
        var patch = Aria_Daemon_V1_SettingsPatch()
        patch.prompts = Aria_Daemon_V1_Prompts()
        patch.apply(to: &snapshot)
        XCTAssertEqual(snapshot.prompts.entries, [])
    }

    func testASpeakingRateLandsOnAStepInsideTheDaemonsRange() {
        XCTAssertEqual(DictationEdits.roundedSpeakingRate(1.13), 1.15, accuracy: 0.0001)
        XCTAssertEqual(DictationEdits.roundedSpeakingRate(0.1), 0.25, accuracy: 0.0001)
        XCTAssertEqual(DictationEdits.roundedSpeakingRate(9), 4, accuracy: 0.0001)
    }
}
