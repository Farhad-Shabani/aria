import XCTest

@testable import Aria

final class SetupProtocolTests: XCTestCase {
    private static func responses() throws -> (event: Aria_Setup_V1_ApplyResponse, summary: Aria_Setup_V1_ApplyResponse)
    {
        var download = Aria_Setup_V1_DownloadProgress()
        download.model = "whisper"
        download.received = 10
        download.total = 20
        var event = Aria_Setup_V1_ApplyResponse()
        event.event.download = download
        var summary = Aria_Setup_V1_ApplyResponse()
        summary.summary.grant = .armed
        return (event, summary)
    }

    /// The child's stdout arrives in arbitrary chunks; frames must come out
    /// whole and in order regardless of where the chunks cut.
    func test_frames_surviveBeingSplitAcrossReads() throws {
        let (event, summary) = try Self.responses()
        let stream = try SetupFrames.encode(event) + SetupFrames.encode(summary)

        for cut in 0...stream.count {
            var reader = SetupFrames.Reader()
            var frames = reader.push(stream.prefix(cut))
            frames += reader.push(stream.suffix(from: cut))

            XCTAssertEqual(frames.count, 2, "cut at \(cut)")
            XCTAssertEqual(try Aria_Setup_V1_ApplyResponse(serializedBytes: frames[0]), event)
            XCTAssertEqual(try Aria_Setup_V1_ApplyResponse(serializedBytes: frames[1]), summary)
        }
    }

    func test_frames_encodeALengthLongerThanOneByte() throws {
        var request = Aria_Setup_V1_ApplyRequest()
        request.downloads = Array(repeating: "a-model-name", count: 40)
        let frame = try SetupFrames.encode(request)

        var reader = SetupFrames.Reader()
        let frames = reader.push(frame)

        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(try Aria_Setup_V1_ApplyRequest(serializedBytes: frames[0]), request)
        XCTAssertTrue(frames[0].count > 127, "the length must have needed two varint bytes")
    }

    func test_outcome_readsTheSummaryTheInterruptOrTheChildsOwnAccount() {
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
}

extension Result {
    fileprivate var failure: Failure? {
        if case .failure(let error) = self { return error }
        return nil
    }
}
