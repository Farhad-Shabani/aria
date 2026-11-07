import XCTest

@testable import Aria

final class SpeechModelsTests: XCTestCase {
    private static func engine(
        _ id: String,
        settable: Bool = true,
        source: Aria_Setup_V1_ModelSource = .downloads
    ) -> Aria_Setup_V1_Choice {
        var engine = Aria_Setup_V1_Choice()
        engine.id = id
        engine.label = id.capitalized
        engine.settable = settable
        engine.modelSource = source
        return engine
    }

    private static func model(_ id: String, files: Aria_Setup_V1_ModelFiles, bytes: UInt64 = 250_000_000)
        -> Aria_Setup_V1_Model
    {
        var model = Aria_Setup_V1_Model()
        model.id = id
        model.engineID = id
        model.files = files
        model.sizeBytes = bytes
        return model
    }

    private static func mac(running current: String) -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.transcriptionChoices = [
            engine("parakeet"),
            engine("moonshine"),
            engine("whisper-candle"),
            engine("nemotron"),
            engine("parakeet-mlx", settable: false, source: .fetchedOnFirstUse),
            engine("server", settable: false, source: .remote),
        ]
        inspection.models = [
            model("parakeet", files: .complete),
            model("moonshine", files: .absent),
            model("whisper-candle", files: .complete),
            model("nemotron", files: .external),
        ]
        inspection.defaults.transcriptionEngineID = current
        return inspection
    }

    func testModelsOnYourMacComeFirstAndTheRestShowWhatTheyDownload() {
        let models = SpeechModels(Self.mac(running: "parakeet"))

        XCTAssertEqual(models.onMac.map(\.id), ["parakeet", "whisper-candle", "nemotron"])
        XCTAssertEqual(models.toDownload.map(\.id), ["moonshine"])
        XCTAssertEqual(models.model("moonshine")?.size, SetupState.size(250_000_000))
    }

    func testAModelSetupCantFetchShowsOnlyWhileItsCurrent() {
        XCTAssertNil(SpeechModels(Self.mac(running: "parakeet")).model("parakeet-mlx"))
        XCTAssertNil(SpeechModels(Self.mac(running: "parakeet")).model("server"))

        let mlx = SpeechModels(Self.mac(running: "parakeet-mlx"))
        XCTAssertNil(mlx.model("parakeet-mlx")?.size)
        XCTAssertEqual(mlx.onMac.map(\.id).last, "parakeet-mlx")
    }
}
