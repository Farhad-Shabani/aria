import XCTest

@testable import Aria

final class ModelTransferTests: XCTestCase {
    func testMoreBytesThanExpectedStillReadAsDone() {
        XCTAssertEqual(ModelTransfer(received: 150, total: 100).fraction, 1)
        XCTAssertEqual(ModelTransfer(received: 50, total: 100).fraction, 0.5)
    }

    func testAReportWithoutASizeGivesNoTransfer() {
        var progress = Aria_Setup_V1_DownloadProgress()
        progress.receivedBytes = 10

        XCTAssertNil(ModelTransfer(progress))
    }
}
