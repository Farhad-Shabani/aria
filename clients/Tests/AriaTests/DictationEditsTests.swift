import XCTest

@testable import Aria

final class DictationEditsTests: XCTestCase {
    func testCopyOutputForcesCompleteDelivery() {
        var patch = Aria_Daemon_V1_PreferencePatch()
        DictationEdits.setOutput(.copy, in: &patch)
        XCTAssertEqual(patch.outputMode, .copy)
        XCTAssertEqual(patch.deliveryMode, .complete)
    }

    func testInsertOutputLeavesDeliveryAlone() {
        var patch = Aria_Daemon_V1_PreferencePatch()
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

    /// The two messages share field numbers, so a protobuf merge is the
    /// reference the hand-written overlay must match field for field.
    func testPatchApplyCoversEveryField() throws {
        var patch = Aria_Daemon_V1_PreferencePatch()
        patch.recognitionLanguage = "de"
        patch.outputMode = .copy
        patch.deliveryMode = .complete
        patch.silenceDurationMs = 2500
        patch.ownVoiceOnly = true
        patch.dictationHotkey = .fn
        patch.askHotkey.chord.keyCode = 7
        patch.promptListHotkey.chord.keyCode = 8
        patch.projectVocabulary = true
        patch.vocabularyDirectory = "/repo"
        patch.dictationHistory = false
        patch.historyAudioRetention = true
        patch.muteOutput = true
        patch.prompts.entries = [PromptEdits.makeEntry(for: URL(fileURLWithPath: "/p.md"))]
        var combined = Aria_Daemon_V1_PreferencePatch()
        patch.apply(to: &combined)
        XCTAssertEqual(combined, patch)
        var overlaid = Aria_Daemon_V1_UserPreferences()
        patch.apply(to: &overlaid)
        var merged = Aria_Daemon_V1_UserPreferences()
        let bytes: [UInt8] = try patch.serializedBytes()
        try merged.merge(serializedBytes: bytes)
        XCTAssertEqual(overlaid, merged)
    }

    func testPatchApplyLaysOnlySetFieldsOverSnapshot() {
        var snapshot = Aria_Daemon_V1_UserPreferences()
        snapshot.outputMode = .insert
        snapshot.muteOutput = true
        snapshot.vocabularyDirectory = "/repo"
        var patch = Aria_Daemon_V1_PreferencePatch()
        patch.outputMode = .copy
        patch.vocabularyDirectory = ""
        patch.apply(to: &snapshot)
        XCTAssertEqual(snapshot.outputMode, .copy)
        XCTAssertTrue(snapshot.muteOutput)
        XCTAssertFalse(snapshot.hasVocabularyDirectory)
    }

    func testPatchApplyReplacesEveryPrompt() {
        var snapshot = Aria_Daemon_V1_UserPreferences()
        snapshot.prompts.entries = [
            PromptEdits.makeEntry(for: URL(fileURLWithPath: "/a.md")),
            PromptEdits.makeEntry(for: URL(fileURLWithPath: "/b.md")),
        ]
        var patch = Aria_Daemon_V1_PreferencePatch()
        patch.prompts = Aria_Daemon_V1_Prompts()
        patch.apply(to: &snapshot)
        XCTAssertEqual(snapshot.prompts.entries, [])
    }
}
