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

    func testPreferencePatchPreservesExplicitFalseAndAbsentProjectFields() throws {
        var patch = Aria_Daemon_V1_PreferencePatch()
        patch.deliveryMode = .complete
        let captions = try Aria_Daemon_V1_PreferencePatch(serializedBytes: patch.serializedData())
        XCTAssertFalse(captions.hasProjectVocabulary)
        XCTAssertFalse(captions.hasVocabularyDirectory)

        patch.projectVocabulary = false
        patch.vocabularyDirectory = "/work/aria"
        let selected = try Aria_Daemon_V1_PreferencePatch(serializedBytes: patch.serializedData())
        XCTAssertTrue(selected.hasProjectVocabulary)
        XCTAssertFalse(selected.projectVocabulary)
        XCTAssertEqual(selected.vocabularyDirectory, "/work/aria")
    }
}
