import XCTest

@testable import Aria

@MainActor
final class SetupStatusTests: XCTestCase {
    private static func missing(_ kind: Aria_Setup_V1_Missing.Kind, model: String = "") -> Aria_Setup_V1_Missing {
        var missing = Aria_Setup_V1_Missing()
        missing.kind = kind
        missing.modelID = model
        return missing
    }

    func testOnlyTheGrantAndTheRecognitionModelAreItemsOnAMacThatTranscribes() {
        var inspection = SetupStateTests.freshMac()
        inspection.missing = [
            Self.missing(.settings),
            Self.missing(.wakeword),
            Self.missing(.model, model: "kokoro"),
            Self.missing(.service),
            Self.missing(.model, model: "parakeet"),
            Self.missing(.grant),
        ]

        XCTAssertEqual(SetupItem.items(in: inspection), [.speechModel(id: "parakeet"), .ariaKey])
    }

    func testASettingsGapIsAnItemOnlyForAServerNobodyPaired() {
        var inspection = SetupStateTests.freshMac()
        inspection.transcriptionRunsOn = .server
        inspection.missing = [Self.missing(.settings)]

        XCTAssertEqual(SetupItem.items(in: inspection), [.server])
        inspection.server = Aria_Setup_V1_PairedServer()
        XCTAssertEqual(SetupItem.items(in: inspection), [])
    }

    func testAnInspectionStartedBeforeAChangeDoesNotLandOverIt() async {
        var onServer = SetupStateTests.freshMac()
        onServer.transcriptionRunsOn = .server
        let gate = Gate()
        let status = SetupStatus { [onThisMac = SetupStateTests.freshMac()] in
            await gate.wait()
            return onThisMac
        }
        let stale = Task { await status.refresh() }
        await gate.reached()

        status.show(onServer)
        gate.open()
        await stale.value

        XCTAssertEqual(status.transcription, .server)
    }

    func testTheSpeakerModelCountsWhenCompleteOrExternal() {
        func inspection(_ change: (inout Aria_Setup_V1_Model) -> Void) -> Aria_Setup_V1_Inspection {
            var inspection = SetupStateTests.freshMac()
            inspection.models = inspection.models.map { model in
                var model = model
                if model.id == SetupStatus.speakerModel { change(&model) }
                return model
            }
            return inspection
        }

        XCTAssertFalse(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.files = .partial }))
        XCTAssertTrue(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.files = .complete }))
        XCTAssertTrue(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.files = .external }))
    }
}
