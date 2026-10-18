import XCTest

@testable import Aria

final class DictationEditsTests: XCTestCase {
    func test_copyOutput_forcesCompleteDelivery() {
        var patch = Aria_Daemon_V1_PreferencePatch()
        DictationEdits.setOutput(.copy, in: &patch)
        XCTAssertEqual(patch.outputMode, .copy)
        XCTAssertEqual(patch.deliveryMode, .complete)
    }

    func test_insertOutput_leavesDeliveryAlone() {
        var patch = Aria_Daemon_V1_PreferencePatch()
        DictationEdits.setOutput(.insert, in: &patch)
        XCTAssertEqual(patch.outputMode, .insert)
        XCTAssertFalse(patch.hasDeliveryMode)
    }

    func test_silenceMilliseconds_roundsToWholeMilliseconds() {
        XCTAssertEqual(DictationEdits.silenceMilliseconds(seconds: 1.5), 1500)
        XCTAssertEqual(DictationEdits.silenceMilliseconds(seconds: 0.2999999), 300)
    }

    func test_languageChoices_appendOnlyAnUnknownStoredCode() {
        XCTAssertEqual(DictationEdits.languageChoices(including: "en"), ["auto", "en"])
        XCTAssertEqual(DictationEdits.languageChoices(including: ""), ["auto", "en"])
        XCTAssertEqual(DictationEdits.languageChoices(including: "pt-br"), ["auto", "en", "pt-br"])
    }

    /// The two messages share field numbers, so a protobuf merge is the
    /// reference the hand-written overlay must match field for field.
    func test_patchApply_coversEveryField() throws {
        var patch = Aria_Daemon_V1_PreferencePatch()
        patch.recognitionLanguage = "de"
        patch.outputMode = .copy
        patch.deliveryMode = .complete
        patch.silenceDurationMs = 2500
        patch.ownVoiceOnly = true
        patch.dictationHotkey = .fn
        patch.askHotkey.enabled = true
        patch.askHotkey.keyCode = 7
        patch.projectVocabulary = true
        patch.vocabularyDirectory = "/repo"
        patch.dictationHistory = false
        patch.historyAudioRetention = true
        patch.muteOutput = true
        var overlaid = Aria_Daemon_V1_UserPreferences()
        patch.apply(to: &overlaid)
        var merged = Aria_Daemon_V1_UserPreferences()
        let bytes: [UInt8] = try patch.serializedBytes()
        try merged.merge(serializedBytes: bytes)
        XCTAssertEqual(overlaid, merged)
    }

    func test_patchApply_laysOnlySetFieldsOverSnapshot() {
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
}
