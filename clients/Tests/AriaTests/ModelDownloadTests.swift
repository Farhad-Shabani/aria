import XCTest

@testable import Aria

@MainActor
final class ModelDownloadTests: XCTestCase {
    @MainActor
    private final class Spy {
        var request: Aria_Setup_V1_ApplyRequest?
        var stops = 0
        var starts = 0
        var downloaded = 0
    }

    /// A Mac whose app runs its own daemon, so a download hands it off.
    private static func appRunMac() -> Aria_Setup_V1_Inspection {
        var inspection = SetupStateTests.freshMac()
        inspection.service.job = .absent
        return inspection
    }

    private func download(
        spy: Spy,
        inspection: Aria_Setup_V1_Inspection = appRunMac(),
        apply:
            @escaping (Aria_Setup_V1_ApplyRequest, @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void)
            async throws -> Aria_Setup_V1_Summary
    ) -> ModelDownload {
        ModelDownload(
            effects: .init(
                inspect: { inspection },
                apply: apply,
                stopDaemon: { spy.stops += 1 },
                startDaemon: { spy.starts += 1 }
            ), runner: SetupRunner())
    }

    private func eventually(_ condition: () -> Bool) async {
        for _ in 0..<1000 where !condition() { await Task.yield() }
    }

    func testADownloadSendsTheChoicesReadWhenItStartsAndReportsItsProgress() async {
        let gate = Gate()
        let spy = Spy()
        let inspection = Self.appRunMac()
        let download = download(spy: spy, inspection: inspection) { request, onEvent in
            await MainActor.run { spy.request = request }
            onEvent(.download(received: 50, of: 100))
            await gate.wait()
            onEvent(.started(.service))
            await gate.wait()
            return Aria_Setup_V1_Summary()
        }

        download.start("parakeet") { spy.downloaded += 1 }
        await gate.reached()
        let halfway = ModelDownload.State.downloading(.init(received: 50, total: 100))
        await eventually { download.state == halfway }
        XCTAssertEqual(download.state, halfway)
        XCTAssertEqual(spy.request?.downloads, ["parakeet"])
        XCTAssertEqual(spy.request?.selections, inspection.defaults)

        gate.open()
        await gate.reached()
        await eventually { download.state == .finishing }
        XCTAssertEqual(download.state, .finishing)

        gate.open()
        await eventually { spy.downloaded == 1 }
        XCTAssertEqual(download.state, .idle)
        XCTAssertEqual([spy.stops, spy.starts], [1, 0], "the service apply installed runs Aria")
    }

    func testACanceledDownloadGoesBackToItsButtonAndRestartsAria() async {
        let gate = Gate()
        let spy = Spy()
        let download = download(spy: spy) { _, _ in
            await gate.wait()
            if Task.isCancelled { throw SetupClientError.cancelled }
            return Aria_Setup_V1_Summary()
        }

        download.start("parakeet") { spy.downloaded += 1 }
        await gate.reached()
        download.cancel()
        gate.open()
        await eventually { spy.starts == 1 }

        XCTAssertEqual(download.state, .idle)
        XCTAssertEqual([spy.stops, spy.starts, spy.downloaded], [1, 1, 0])
    }

    func testOnlyTheDownloadingModelsRowShowsTheRun() {
        let download = download(spy: Spy()) { _, _ in Aria_Setup_V1_Summary() }

        download.start("parakeet") {}

        XCTAssertEqual(download.state(of: "parakeet"), .downloading(nil))
        XCTAssertEqual(download.state(of: "segmentation"), .idle)
    }

    func testTheRunStaysBusyUntilItsFollowUpFinishes() async {
        let spy = Spy()
        var busyDuringFollowUp: Bool?
        let download = download(spy: spy) { _, _ in Aria_Setup_V1_Summary() }

        download.start("parakeet") {
            busyDuringFollowUp = download.isRunning
            spy.downloaded += 1
        }
        await eventually { spy.downloaded == 1 }

        XCTAssertEqual(busyDuringFollowUp, true)
        XCTAssertEqual(download.state, .idle)
    }

    func testAFailedDownloadSaysSoAndRestartsAria() async {
        let spy = Spy()
        let download = download(spy: spy) { _, _ in throw SetupClientError.failed("disk full") }

        download.start("parakeet") { spy.downloaded += 1 }
        await eventually { download.state == .failed(ModelDownload.failure) }

        XCTAssertEqual(download.state, .failed(ModelDownload.failure))
        XCTAssertEqual([spy.stops, spy.starts, spy.downloaded], [1, 1, 0])
    }
}
