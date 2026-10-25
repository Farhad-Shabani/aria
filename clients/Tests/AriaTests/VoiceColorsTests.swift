import XCTest

@testable import Aria

@MainActor
final class VoiceColorsTests: XCTestCase {
    private func defaults() -> UserDefaults {
        UserDefaults(suiteName: "VoiceColorsTests-\(UUID())")!
    }

    func test_eachNewVoiceTakesTheLeastWornColor() {
        XCTAssertEqual(VoiceColors.next(after: []), .blue)
        XCTAssertEqual(VoiceColors.next(after: [.blue, .gold]), .clay)
        XCTAssertEqual(VoiceColors.next(after: VoiceColor.palette + [.blue]), .clay)
        XCTAssertEqual(VoiceColors.next(after: [.plain]), .blue, "None is chosen, never handed out")
    }

    func test_syncFreesTheColorOfARemovedVoice() {
        let colors = VoiceColors(defaults: defaults())
        colors.sync(enrolled: ["farhad", "sam"])
        XCTAssertEqual(colors.color(of: "sam"), .clay)

        colors.sync(enrolled: ["farhad"])
        colors.assignColor(to: "ada")

        XCTAssertNil(colors.color(of: "sam"))
        XCTAssertEqual(colors.color(of: "ada"), .clay)
    }

    func test_aChosenColorOutlivesTheApp() {
        let store = defaults()
        let colors = VoiceColors(defaults: store)
        colors.assignColor(to: "farhad")
        colors.set(.violet, for: "farhad")

        XCTAssertEqual(VoiceColors(defaults: store).color(of: "farhad"), .violet)
    }
}
