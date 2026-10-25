import XCTest

@testable import Aria

final class SetupProgressTests: XCTestCase {
    private func walk(_ steps: [SetupState.Step]) -> SetupProgress {
        var progress = SetupProgress()
        for step in steps { progress.record(step) }
        return progress
    }

    func testExpressSetupSkipsTheEngines() {
        let progress = walk([.inspecting, .welcome, .applying])

        XCTAssertEqual(progress.status(of: .welcome), .done)
        XCTAssertEqual(progress.status(of: .engines), .skipped)
        XCTAssertEqual(progress.status(of: .install), .current)
        XCTAssertEqual(progress.status(of: .hotkey), .upcoming)
        XCTAssertEqual(progress.fraction, 2.0 / 6.0)
    }

    func testGoingBackLeavesLaterStagesUpcoming() {
        let progress = walk([.inspecting, .welcome, .choices, .welcome])

        XCTAssertEqual(progress.status(of: .welcome), .current)
        XCTAssertEqual(progress.status(of: .engines), .upcoming)
    }

    func testFailureMarksTheLastStageReached() {
        let progress = walk([.inspecting, .welcome, .choices, .applying, .failed("boom")])

        XCTAssertEqual(progress.status(of: .engines), .done)
        XCTAssertEqual(progress.status(of: .install), .failed)
        XCTAssertEqual(progress.status(of: .hotkey), .upcoming)
        XCTAssertEqual(progress.fraction, 2.0 / 6.0)
    }

    func testFinishingMarksStagesNeverReachedAsSkipped() {
        let progress = walk([.inspecting, .welcome, .applying, .tryOut, .done])

        XCTAssertEqual(progress.status(of: .install), .done)
        XCTAssertEqual(progress.status(of: .hotkey), .skipped)
        XCTAssertEqual(progress.status(of: .tryOut), .done)
        XCTAssertEqual(progress.fraction, 1)
    }

    func testInspectingForgetsTheEarlierWalk() {
        let progress = walk([.inspecting, .welcome, .choices, .failed("boom"), .inspecting, .welcome, .applying])

        XCTAssertEqual(progress.status(of: .engines), .skipped)
    }
}
