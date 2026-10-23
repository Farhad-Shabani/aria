import XCTest

@testable import Aria

final class SetupProgressTests: XCTestCase {
    private func walk(_ steps: [SetupState.Step]) -> SetupProgress {
        var progress = SetupProgress()
        for step in steps { progress.record(step) }
        return progress
    }

    func test_expressSetup_skipsTheEngines() {
        let progress = walk([.inspecting, .welcome, .applying])

        XCTAssertEqual(progress.status(of: .welcome), .done)
        XCTAssertEqual(progress.status(of: .engines), .skipped)
        XCTAssertEqual(progress.status(of: .install), .current)
        XCTAssertEqual(progress.status(of: .hotkey), .upcoming)
        XCTAssertEqual(progress.fraction, 2.0 / 6.0)
    }

    func test_goingBack_leavesLaterStagesUpcoming() {
        let progress = walk([.inspecting, .welcome, .choices, .welcome])

        XCTAssertEqual(progress.status(of: .welcome), .current)
        XCTAssertEqual(progress.status(of: .engines), .upcoming)
    }

    func test_failure_marksTheLastStageReached() {
        let progress = walk([.inspecting, .welcome, .choices, .applying, .failed("boom")])

        XCTAssertEqual(progress.status(of: .engines), .done)
        XCTAssertEqual(progress.status(of: .install), .failed)
        XCTAssertEqual(progress.status(of: .hotkey), .upcoming)
        XCTAssertEqual(progress.fraction, 2.0 / 6.0)
    }

    func test_finishing_marksStagesNeverReachedAsSkipped() {
        let progress = walk([.inspecting, .welcome, .applying, .tryOut, .done])

        XCTAssertEqual(progress.status(of: .install), .done)
        XCTAssertEqual(progress.status(of: .hotkey), .skipped)
        XCTAssertEqual(progress.status(of: .tryOut), .done)
        XCTAssertEqual(progress.fraction, 1)
    }

    func test_inspecting_forgetsTheEarlierWalk() {
        let progress = walk([.inspecting, .welcome, .choices, .failed("boom"), .inspecting, .welcome, .applying])

        XCTAssertEqual(progress.status(of: .engines), .skipped)
    }
}
