import XCTest

@testable import Aria

@MainActor
final class SttEngineChangeTests: XCTestCase {
    private func eventually(_ condition: () -> Bool) async {
        for _ in 0..<1000 where !condition() { await Task.yield() }
    }

    func testAChangeFollowsSetupThroughTheDownloadAndTheWrite() async {
        let gate = Gate()
        var requested: Aria_Setup_V1_SetSttEngineRequest?
        var ended = 0
        let model = SttEngineChange(
            effects: .init(run: { request, onEvent in
                await MainActor.run { requested = request }
                onEvent(.started(.models))
                onEvent(.download(received: 50, of: 100))
                await gate.wait()
                onEvent(.started(.settings))
                await gate.wait()
                return Aria_Setup_V1_SetSttEngineSummary()
            }), runner: SetupRunner())

        model.start("moonshine") {
            XCTAssertEqual(model.engine, "moonshine", "the picker shows the new engine until settings catch up")
            ended += 1
        }
        XCTAssertEqual(model.state, .finishing)
        await gate.reached()
        let halfway = SttEngineChange.State.downloading(.init(received: 50, total: 100))
        await eventually { model.state == halfway }
        XCTAssertEqual(model.state, halfway)
        XCTAssertEqual(requested?.engineID, "moonshine")

        gate.open()
        await gate.reached()
        await eventually { model.state == .finishing }
        XCTAssertEqual(model.state, .finishing)

        gate.open()
        await eventually { ended == 1 }
        XCTAssertEqual(model.state, .idle)
        XCTAssertNil(model.engine)
    }

    func testACanceledChangeStillRefreshesSinceTheWriteMayHaveLanded() async {
        let gate = Gate()
        var ended = 0
        let model = SttEngineChange(
            effects: .init(run: { _, _ in
                await gate.wait()
                if Task.isCancelled { throw SetupClientError.cancelled }
                return Aria_Setup_V1_SetSttEngineSummary()
            }), runner: SetupRunner())

        model.start("moonshine") { ended += 1 }
        await gate.reached()
        model.cancel()
        gate.open()
        await eventually { model.state == .idle }

        XCTAssertEqual(model.state, .idle)
        XCTAssertNil(model.engine)
        XCTAssertEqual(ended, 1)
    }

    func testAFailedChangeKeepsItsEngineSoItsRowCanSaySo() async {
        var ended = 0
        let model = SttEngineChange(
            effects: .init(run: { _, _ in throw SetupClientError.failed("offline") }), runner: SetupRunner())

        model.start("moonshine") { ended += 1 }
        await eventually { ended == 1 }

        XCTAssertEqual(model.state, .failed(SttEngineChange.failure))
        XCTAssertEqual(model.engine, "moonshine")
    }

    func testAChangeWhileAnotherSetupRunsSaysSoAndStartsNothing() async {
        let gate = Gate()
        let runner = SetupRunner()
        var runs = 0
        let model = SttEngineChange(
            effects: .init(run: { _, _ in
                runs += 1
                return Aria_Setup_V1_SetSttEngineSummary()
            }), runner: runner)
        let other = Task { try await runner.run { await gate.wait() } }
        await gate.reached()

        var ended = 0
        model.start("moonshine") { ended += 1 }
        await eventually { ended == 1 }

        XCTAssertEqual(model.state, .failed(SetupClientError.busyMessage))
        XCTAssertEqual(runs, 0)
        gate.open()
        _ = try? await other.value
    }
}
