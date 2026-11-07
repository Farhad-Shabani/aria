import XCTest

@testable import Aria

final class ListenTranscriptTests: XCTestCase {
    private func event(
        _ stage: Aria_Daemon_V1_ListenStage, text: String = "", speakerLabel: String = "",
        at offset: UInt64 = 0, mergedFrom: String = ""
    ) -> Aria_Daemon_V1_ListenEvent {
        var event = Aria_Daemon_V1_ListenEvent()
        event.stage = stage
        event.text = text
        event.speakerLabel = speakerLabel
        event.offset = .init(seconds: Int64(offset))
        event.mergedFromLabel = mergedFrom
        return event
    }

    private let start = Date(timeIntervalSince1970: 1_000)

    func testSegmentsBecomeLinesAndCopyAsTheFileWritesThem() {
        var transcript = ListenTranscript()
        transcript.apply(event(.segmentTranscribed, text: " Morning. ", speakerLabel: "Speaker 1", at: 3), at: start)
        transcript.apply(event(.segmentTranscribed, text: "   ", at: 4), at: start)
        transcript.apply(event(.segmentTranscribed, text: "Hi.", at: 3725), at: start)

        XCTAssertEqual(transcript.text, "[00:00:03] Speaker 1: Morning.\n\n[01:02:05] Hi.")
        XCTAssertEqual(transcript.offsetSeconds, 3725)
    }

    func testOneSpeakersWordsShareALineUntilAPause() {
        var transcript = ListenTranscript()
        let say = { (text: String, seconds: TimeInterval) in
            transcript.apply(
                self.event(.segmentTranscribed, text: text, speakerLabel: "Speaker 1"),
                at: self.start.addingTimeInterval(seconds))
        }
        say("One", 0)
        say("two.", 2)
        say("Three.", 2 + ListenTranscript.paragraphGap)

        XCTAssertEqual(transcript.lines.map(\.text), ["One two.", "Three."])
    }

    func testAMergedSpeakerRelabelsEarlierLines() {
        var transcript = ListenTranscript()
        transcript.apply(event(.segmentTranscribed, text: "One.", speakerLabel: "Speaker 2"), at: start)
        transcript.apply(event(.segmentTranscribed, text: "Two.", speakerLabel: "Speaker 1"), at: start)

        transcript.apply(event(.speakerMerged, speakerLabel: "Speaker 1", mergedFrom: "Speaker 2"), at: start)

        XCTAssertEqual(transcript.lines.map(\.speakerLabel), ["Speaker 1", "Speaker 1"])
    }

    func testANewSessionStartsAnEmptyTranscript() {
        var transcript = ListenTranscript()
        transcript.apply(event(.segmentTranscribed, text: "Old.", at: 90), at: start)

        transcript.apply(event(.started), at: start)

        XCTAssertEqual(transcript, ListenTranscript())
    }

    func testOnlyTheNewestLinesAreKept() {
        var transcript = ListenTranscript()
        for index in 0...ListenTranscript.lineLimit {
            let arrived = start.addingTimeInterval(Double(index) * ListenTranscript.paragraphGap)
            transcript.apply(event(.segmentTranscribed, text: "Line \(index).", at: UInt64(index)), at: arrived)
        }

        XCTAssertEqual(transcript.lines.count, ListenTranscript.lineLimit)
        XCTAssertEqual(transcript.lines.first?.text, "Line 1.")
    }
}
