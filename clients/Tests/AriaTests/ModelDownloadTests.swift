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

    private static func progress(received: UInt64, of total: UInt64) -> Aria_Setup_V1_SetupEvent {
        var progress = Aria_Setup_V1_DownloadProgress()
        progress.modelID = "parakeet"
        progress.receivedBytes = received
        progress.totalBytes = total
        var event = Aria_Setup_V1_SetupEvent()
        event.download = progress
        return event
    }

    private static func started(_ step: Aria_Setup_V1_Step) -> Aria_Setup_V1_SetupEvent {
        var event = Aria_Setup_V1_SetupEvent()
        event.started = step
        return event
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
            ))
    }

    private func eventually(_ condition: () -> Bool) async {
        for _ in 0..<1000 where !condition() { await Task.yield() }
    }

    func testADownloadSendsTheChoicesReadWhenItStartsAndReportsItsProgress() async {
        let gate = Gate()
        let spy = Spy()
        var inspection = Self.appRunMac()
        inspection.defaults.ariaKey = false
        let download = download(spy: spy, inspection: inspection) { request, onEvent in
            await MainActor.run { spy.request = request }
            onEvent(Self.progress(received: 50, of: 100))
            await gate.wait()
            onEvent(Self.started(.service))
            await gate.wait()
            return Aria_Setup_V1_Summary()
        }

        download.start("parakeet") { spy.downloaded += 1 }
        await gate.reached()
        await eventually { download.state == .downloading(fraction: 0.5) }
        XCTAssertEqual(download.state, .downloading(fraction: 0.5))
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

    func testAFailedDownloadSaysSoAndRestartsAria() async {
        let spy = Spy()
        let download = download(spy: spy) { _, _ in throw SetupClientError.failed("disk full") }

        download.start("parakeet") { spy.downloaded += 1 }
        await eventually { download.state == .failed }

        XCTAssertEqual(download.state, .failed)
        XCTAssertEqual([spy.stops, spy.starts, spy.downloaded], [1, 1, 0])
    }
}
