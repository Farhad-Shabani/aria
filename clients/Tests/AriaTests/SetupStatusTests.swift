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

    func test_onlyTheGrantAndTheRecognitionModelAreRequired() {
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
}
