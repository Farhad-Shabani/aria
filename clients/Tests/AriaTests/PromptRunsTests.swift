import XCTest

@testable import Aria

final class PromptRunsTests: XCTestCase {
    func test_aRunIsNamedByItsRecordingAndItsTicket() {
        var runs = PromptRuns()
        runs.add(.recording(7), prompt: "Polish")
        runs.add(.ticket(3), prompt: "Polish")

        XCTAssertEqual(runs.prompt(of: .recording(7)), "Polish")
        XCTAssertEqual(runs.prompt(of: .ticket(3)), "Polish")
        XCTAssertNil(runs.prompt(of: .recording(3)))
    }

    func test_onlyTheLatestRunsAreKept() {
        var runs = PromptRuns()
        for ticket in UInt64(1)...40 {
            runs.add(.ticket(ticket), prompt: "Polish")
        }

        XCTAssertNil(runs.prompt(of: .ticket(1)))
        XCTAssertEqual(runs.prompt(of: .ticket(40)), "Polish")
    }
}
