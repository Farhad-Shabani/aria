import XCTest

@testable import Aria

final class SetupProtocolTests: XCTestCase {
    func testFramesRejectMalformedLengths() throws {
        for lastByte: UInt8 in [2, 0x80, 0xff] {
            var reader = SetupFrames.Reader()
            let prefix = Data(repeating: 0x80, count: 9)
            XCTAssertTrue(try reader.push(prefix).isEmpty)
            XCTAssertThrowsError(try reader.push(Data([lastByte]))) { error in
                XCTAssertEqual(error as? SetupFrames.FrameError, .malformedLength)
            }
        }
    }

    func testFramesRejectOversizedLengths() throws {
        let lengths: [[UInt8]] = [
            [0x80, 0x80, 0x80, 0x80, 0x08],
            Array(repeating: 0xff, count: 8) + [0x7f],
            Array(repeating: 0xff, count: 9) + [0x01],
        ]
        for length in lengths {
            var reader = SetupFrames.Reader()
            XCTAssertThrowsError(try reader.push(Data(length))) { error in
                XCTAssertEqual(error as? SetupFrames.FrameError, .messageTooLarge)
            }
        }
    }

    func testFramesAcceptTheMaximumLengthWithoutAllocatingItsPayload() throws {
        var reader = SetupFrames.Reader()
        XCTAssertTrue(try reader.push(Data([0xff, 0xff, 0xff, 0xff, 0x07])).isEmpty)
        XCTAssertThrowsError(try reader.finish()) { error in
            XCTAssertEqual(error as? SetupFrames.FrameError, .truncated)
        }
    }

    func testFramesFinishRejectsPartialPrefixesAndPayloads() throws {
        for bytes in [Data([0x80]), Data([2, 0x01]), Data([0, 2, 0x01])] {
            var reader = SetupFrames.Reader()
            _ = try reader.push(bytes)
            XCTAssertThrowsError(try reader.finish()) { error in
                XCTAssertEqual(error as? SetupFrames.FrameError, .truncated)
            }
        }
    }

    func testFramesAcceptEmptyAndNoncanonicalFrames() throws {
        var reader = SetupFrames.Reader()
        XCTAssertNoThrow(try reader.finish())
        let frames = try reader.push(Data([0, 0x80, 0, 1, 0x42]))
        XCTAssertEqual(frames, [Data(), Data(), Data([0x42])])
        XCTAssertNoThrow(try reader.finish())
    }

    /// The child's stdout arrives in arbitrary chunks; frames must come out
    /// whole and in order regardless of where the chunks cut.
    func testFramesSurviveBeingSplitAcrossReads() throws {
        var download = Aria_Setup_V1_DownloadProgress()
        download.model = "whisper"
        download.received = 10
        download.total = 20
        var event = Aria_Setup_V1_ApplyResponse()
        event.event.download = download
        var summary = Aria_Setup_V1_ApplyResponse()
        summary.summary.grant = .armed
        let stream = try SetupFrames.encode(event) + SetupFrames.encode(summary)

        for cut in 0...stream.count {
            var reader = SetupFrames.Reader()
            var frames = try reader.push(stream.prefix(cut))
            frames += try reader.push(stream.suffix(from: cut))
            try reader.finish()

            XCTAssertEqual(frames.count, 2, "cut at \(cut)")
            XCTAssertEqual(try Aria_Setup_V1_ApplyResponse(serializedBytes: frames[0]), event)
            XCTAssertEqual(try Aria_Setup_V1_ApplyResponse(serializedBytes: frames[1]), summary)
        }
    }

    func testFramesEncodeALengthLongerThanOneByte() throws {
        var request = Aria_Setup_V1_ApplyRequest()
        request.downloads = Array(repeating: "a-model-name", count: 40)
        let frame = try SetupFrames.encode(request)

        var reader = SetupFrames.Reader()
        let frames = try reader.push(frame)

        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(try Aria_Setup_V1_ApplyRequest(serializedBytes: frames[0]), request)
        XCTAssertTrue(frames[0].count > 127, "the length must have needed two varint bytes")
    }

    func testOutcomeReadsTheSummaryTheInterruptOrTheChildsOwnAccount() {
        var summary = Aria_Setup_V1_Summary()
        summary.grant = .waiting
        let finished = SetupClient.Run(summary: summary, status: 0, stderr: Data())
        XCTAssertEqual(try SetupClient.outcome(finished, cancelled: false).get(), summary)

        let interrupted = SetupClient.Run(summary: nil, status: 1, stderr: Data("interrupted\n".utf8))
        XCTAssertEqual(SetupClient.outcome(interrupted, cancelled: true).failure, .cancelled)
        XCTAssertEqual(SetupClient.outcome(interrupted, cancelled: false).failure, .failed("interrupted"))

        let silent = SetupClient.Run(summary: nil, status: 9, stderr: Data())
        XCTAssertEqual(SetupClient.outcome(silent, cancelled: false).failure, .failed("aria setup exited with 9"))
    }

    func testOutcomeReportsTruncationWithoutHidingCancellationOrChildFailure() {
        var run = SetupClient.Run(summary: nil, status: 0, stderr: Data(), frameError: .truncated)
        XCTAssertEqual(SetupClient.outcome(run, cancelled: false).failure, .invalidOutput(.truncated))
        XCTAssertEqual(SetupClient.outcome(run, cancelled: true).failure, .cancelled)
        run.status = 1
        run.stderr = Data("download failed".utf8)
        XCTAssertEqual(SetupClient.outcome(run, cancelled: false).failure, .failed("download failed"))
        let summary = Aria_Setup_V1_Summary()
        run.summary = summary
        XCTAssertEqual(try SetupClient.outcome(run, cancelled: true).get(), summary)
    }
}

extension Result {
    fileprivate var failure: Failure? {
        if case .failure(let error) = self { return error }
        return nil
    }
}
