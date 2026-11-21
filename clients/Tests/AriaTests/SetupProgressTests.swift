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
        XCTAssertEqual(progress.caption, "Step 3 of 7", "a skipped step still counts")
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
        XCTAssertEqual(progress.caption, "Step 3 of 7", "a failure stays on its step")
    }

    func testFinishingMarksStagesNeverReachedAsSkipped() {
        let progress = walk([.inspecting, .welcome, .applying, .tryOut, .done])

        XCTAssertEqual(progress.status(of: .install), .done)
        XCTAssertEqual(progress.status(of: .hotkey), .skipped)
        XCTAssertEqual(progress.status(of: .tryOut), .done)
        XCTAssertEqual(progress.caption, "Done")
    }

    func testInspectingForgetsTheEarlierWalk() {
        let progress = walk([.inspecting, .welcome, .choices, .failed("boom"), .inspecting, .welcome, .applying])

        XCTAssertEqual(progress.status(of: .engines), .skipped)
    }
}
