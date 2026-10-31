import SwiftProtobuf
import XCTest

@testable import Aria

final class VocabularyProtocolTests: XCTestCase {
    func testDictationProjectRoundTripsDirectoryAndExplicitDisable() throws {
        var request = Aria_Daemon_V1_StartDictationRequest()
        request.projectDirectory = "/work/aria"
        let selected = try Aria_Daemon_V1_StartDictationRequest(
            serializedBytes: request.serializedData())
        XCTAssertEqual(selected.project, .projectDirectory("/work/aria"))

        request.projectVocabularyDisabled = .init()
        let disabled = try Aria_Daemon_V1_StartDictationRequest(
            serializedBytes: request.serializedData())
        XCTAssertEqual(disabled.project, .projectVocabularyDisabled(.init()))
        XCTAssertEqual(disabled.projectDirectory, "")
        XCTAssertNil(Aria_Daemon_V1_StartDictationRequest().project)
    }

    func testSettingsPatchPreservesExplicitFalseAndAbsentProjectFields() throws {
        var patch = Aria_Daemon_V1_SettingsPatch()
        patch.deliveryMode = .complete
        let captions = try Aria_Daemon_V1_SettingsPatch(serializedBytes: patch.serializedData())
        XCTAssertFalse(captions.hasVocabularyEnabled)
        XCTAssertFalse(captions.hasVocabularyDir)

        patch.vocabularyEnabled = false
        patch.vocabularyDir = "/work/aria"
        let selected = try Aria_Daemon_V1_SettingsPatch(serializedBytes: patch.serializedData())
        XCTAssertTrue(selected.hasVocabularyEnabled)
        XCTAssertFalse(selected.vocabularyEnabled)
        XCTAssertEqual(selected.vocabularyDir, "/work/aria")
    }
}
