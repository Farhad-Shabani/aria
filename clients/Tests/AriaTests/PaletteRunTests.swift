import XCTest

@testable import Aria

@MainActor
final class PaletteRunTests: XCTestCase {
    @MainActor
    private final class Harness {
        var started: [(UInt64, PromptSource)] = []
        var cancelled: [UInt64] = []
        var ticket: CheckedContinuation<UInt64, Error>?

        var effects: PaletteRun.Effects {
            .init(
                run: { entry, source in
                    self.started.append((entry, source))
                    return try await withCheckedThrowingContinuation { self.ticket = $0 }
                },
                cancel: { self.cancelled.append($0) })
        }
    }

    private static let input = HistoryItem(
        id: 1, created: Date(timeIntervalSince1970: 0), text: "Stand", status: .success)

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<200 where !condition() { await Task.yield() }
        XCTAssertTrue(condition(), file: file, line: line)
    }

    private func startSummary(_ run: PaletteRun, _ harness: Harness) async {
        run.start(.stored(id: "summarize"), named: "Summarize", on: Self.input)
        await waitUntil { harness.ticket != nil }
    }

    func testAPromptRunsOnTheEntryByIDAndItsTicketIsLeftToThePalette() async {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)

        run.start(.stored(id: "summarize"), named: "Summarize", on: Self.input)
        await waitUntil { harness.ticket != nil }
        harness.ticket?.resume(returning: 7)
        await waitUntil { run.result?.ticket == 7 }

        XCTAssertEqual(harness.started.first?.0, 1)
        XCTAssertTrue(run.shows(ticket: 7))
    }

    func testTryAgainRunsAOneOffWithTheSameInstructions() async throws {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)
        let source = PromptSource.oneOff(instructions: "List the action items.")
        run.start(source, named: "List the action items.", on: Self.input)
        await waitUntil { harness.ticket != nil }
        harness.ticket?.resume(returning: 9)
        harness.ticket = nil
        await waitUntil { run.result?.ticket == 9 }
        var failed = Aria_Daemon_V1_ChatEvent()
        failed.ticket = 9
        failed.stage = .failed
        run.apply(failed)

        run.retry()
        await waitUntil { harness.started.count == 2 }

        XCTAssertEqual(harness.started.map(\.1), [source, source])
        harness.ticket?.resume(returning: 10)
    }

    func testASavedResultShowsAsARepliedRunThatCannotRunAgain() {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)
        let saved = PromptResult(id: 3, created: .now, promptName: "Summarize", text: "Short.")

        run.show(saved: saved, on: Self.input)
        run.retry()

        XCTAssertEqual(run.result?.state, .replied("Short."))
        XCTAssertTrue(harness.started.isEmpty)
    }

    func testATicketThatComesAfterTheRunWasCancelledIsCancelledToo() async {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)
        await startSummary(run, harness)

        run.cancel()
        harness.ticket?.resume(returning: 7)

        await waitUntil { harness.cancelled == [7] }
        XCTAssertNil(run.result)
        XCTAssertTrue(run.shows(ticket: 7), "its late events stay out of the pill")
    }

    func testARunRefusedByADaemonShuttingDownSaysAriaIsntRunning() async {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)

        await startSummary(run, harness)
        harness.ticket?.resume(throwing: DaemonFailure.refused(.shuttingDown))
        await waitUntil { run.result?.isRunning == false }

        XCTAssertEqual(run.result?.state, .failed(.init(DaemonFailure.offlineMessage)))
    }

    func testAReleasedRunKeepsRunningWhetherItsTicketCameOrNot() async {
        let harness = Harness()
        let run = PaletteRun(effects: harness.effects)
        await startSummary(run, harness)
        harness.ticket?.resume(returning: 7)
        harness.ticket = nil
        await waitUntil { run.result?.ticket == 7 }

        run.release()
        await startSummary(run, harness)
        run.release()
        harness.ticket?.resume(returning: 8)

        await waitUntil { run.shows(ticket: 8) }
        XCTAssertNil(run.result)
        XCTAssertEqual(harness.cancelled, [], "the daemon saves both replies")
        XCTAssertTrue(run.shows(ticket: 7), "their events stay out of the pill")
    }
}
