import XCTest

@testable import Aria

final class DownloadedModelsTests: XCTestCase {
    private static func model(
        _ id: String,
        files: Aria_Setup_V1_ModelFiles = .complete,
        bytes: UInt64,
        usedBy: [Aria_Setup_V1_ModelUse] = []
    ) -> Aria_Setup_V1_Model {
        var model = Aria_Setup_V1_Model()
        model.id = id
        model.label = "\(id) label"
        model.files = files
        model.sizeBytes = bytes
        model.usedBy = usedBy
        return model
    }

    private static func mac() -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.models = [
            model("whisper", bytes: 875_000_000, usedBy: [.stt]),
            model("nemotron", files: .partial, bytes: 2_500_000_000),
            model("kokoro", bytes: 354_000_000),
            model("parakeet", bytes: 2_500_000_000, usedBy: [.stt, .listen]),
            model("moonshine", files: .absent, bytes: 67_000_000),
            model("speaker", bytes: 27_000_000, usedBy: [.speaker]),
            model("external", files: .external, bytes: 9_000_000_000),
        ]
        return inspection
    }

    func testOnlyModelsOnYourMacShowLargestFirstWithPartialDownloadsLast() {
        let models = DownloadedModels(Self.mac())

        XCTAssertEqual(models.models.map(\.id), ["parakeet", "whisper", "kokoro", "speaker", "nemotron"])
        XCTAssertEqual(models.total, 2_500_000_000 + 875_000_000 + 354_000_000 + 27_000_000)
    }

    func testAModelSaysWhatUsesIt() {
        let models = DownloadedModels(Self.mac())
        let model = { (id: String) in models.models.first { $0.id == id } }

        XCTAssertEqual(model("parakeet")?.detail, "\(SetupState.size(2_500_000_000)) · Used by Dictation and Mac Audio")
        XCTAssertEqual(model("nemotron")?.detail, "Partly downloaded")
        XCTAssertEqual(model("kokoro")?.usedBy, [], "nothing loads it, so it can be deleted")
    }
}

@MainActor
final class ModelRemovalTests: XCTestCase {
    private let kokoro = DownloadedModels.Model(
        id: "kokoro", name: "Kokoro", bytes: 354_000_000, isPartial: false, usedBy: [])

    func testADeletionRefreshesTheList() async {
        var deleted: [String] = []
        var refreshes = 0
        let removal = ModelRemoval { deleted.append($0) }

        await removal.remove(kokoro, using: SetupRunner()) { refreshes += 1 }

        XCTAssertEqual(deleted, ["kokoro"])
        XCTAssertEqual(refreshes, 1)
        XCTAssertNil(removal.failure)
        XCTAssertNil(removal.removing)
    }

    func testAFailedDeletionSaysSoAndStillRefreshes() async {
        var refreshes = 0
        let removal = ModelRemoval { _ in throw SetupClientError.failed("in use") }

        await removal.remove(kokoro, using: SetupRunner()) { refreshes += 1 }

        XCTAssertEqual(removal.failure, "Aria couldn’t delete Kokoro. Try again.")
        XCTAssertEqual(refreshes, 1)
    }
}
