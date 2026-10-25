import XCTest

@testable import Aria

@MainActor
final class SetupStatusTests: XCTestCase {
    private static func missing(_ kind: Aria_Setup_V1_Missing.Kind, model: String = "") -> Aria_Setup_V1_Missing {
        var missing = Aria_Setup_V1_Missing()
        missing.kind = kind
        missing.model = model
        return missing
    }

    func testOnlyTheGrantAndTheRecognitionModelAreRequired() {
        var inspection = SetupStateTests.freshMac()
        inspection.missing = [
            Self.missing(.config),
            Self.missing(.wakeword),
            Self.missing(.model, model: "kokoro"),
            Self.missing(.service),
            Self.missing(.model, model: "parakeet"),
            Self.missing(.grant),
        ]

        XCTAssertEqual(
            SetupStatus.required(in: inspection),
            [Self.missing(.model, model: "parakeet"), Self.missing(.grant)]
        )
    }

    func testTheSpeakerModelCountsWhenCompleteOrExternal() {
        func inspection(_ change: (inout Aria_Setup_V1_Model) -> Void) -> Aria_Setup_V1_Inspection {
            var inspection = SetupStateTests.freshMac()
            inspection.models = inspection.models.map { model in
                var model = model
                if model.name == SetupStatus.speakerModel { change(&model) }
                return model
            }
            return inspection
        }

        XCTAssertFalse(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.onDisk = .partial }))
        XCTAssertTrue(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.onDisk = .complete }))
        XCTAssertTrue(SetupStatus.hasSpeakerModelOnDisk(in: inspection { $0.external = true }))
    }
}
