import XCTest

@testable import Aria

final class HistoryItemTests: XCTestCase {
    private static func wire(_ section: Aria_Daemon_V1_HistorySection) -> Aria_Daemon_V1_HistoryEntry {
        var wire = Aria_Daemon_V1_HistoryEntry()
        wire.createTime = .init(seconds: 0)
        wire.id = 1
        wire.section = section
        wire.source = .local
        wire.status = .success
        wire.audioAvailability = .unavailable
        return wire
    }

    func testTranscriptAndChatEntriesKeepWhatOnlyTheyCarry() throws {
        var systemTranscription = Self.wire(.systemTranscription)
        systemTranscription.text = "[00:00:01] heard"
        systemTranscription.transcript = .with {
            $0.path = "/Users/me/.aria/transcripts/a.txt"
            $0.read = .truncated
        }
        let session = try HistoryItem(systemTranscription)
        XCTAssertEqual(
            session.kind, .systemTranscription(transcript: "/Users/me/.aria/transcripts/a.txt", read: .truncated))
        XCTAssertEqual(session.origin, "Transcript", "no length, so it names its kind")
        systemTranscription.duration = .init(seconds: 2_520)
        XCTAssertEqual(try HistoryItem(systemTranscription).origin, "42 min")
        XCTAssertFalse(session.facts.contains("words"), "only part of the file was read")

        var chat = Self.wire(.chat)
        chat.text = "When is lunch?"
        chat.reply = "Noon."
        let asked = try HistoryItem(chat)
        XCTAssertEqual(asked.kind, .chat(reply: "Noon."))
        XCTAssertEqual(asked.copyText, "Noon.")
        XCTAssertEqual(asked.origin, "Chat", "no folder, so it names its kind")
        chat.projectDirectory = "/Users/me/code/aria"
        XCTAssertEqual(try HistoryItem(chat).origin, "aria")
    }

    func testAnEntryFromADaemonBeforeSectionsIsADictation() throws {
        XCTAssertEqual(try HistoryItem(Self.wire(.unspecified)).kind, .dictation)
        var stray = Self.wire(.unspecified)
        stray.reply = "stray"
        XCTAssertThrowsError(try HistoryItem(stray))
    }

    func testASectionWithoutItsDetailsIsInvalid() {
        XCTAssertThrowsError(try HistoryItem(Self.wire(.systemTranscription)))
        XCTAssertThrowsError(try HistoryItem(Self.wire(.chat)))
        var dictation = Self.wire(.dictation)
        dictation.reply = "stray"
        XCTAssertThrowsError(try HistoryItem(dictation))
    }

    func testFactsOmitUnknownDurationAndAnIncompleteWordCount() throws {
        var wire = Self.wire(.dictation)
        wire.source = .relay
        wire.status = .transcriptionFailed
        let item = try HistoryItem(wire)
        XCTAssertEqual(item.facts, "iPhone or Apple Watch")
        XCTAssertEqual(item.source, .relay)
        XCTAssertEqual(item.source.title, "iPhone or Apple Watch")
        XCTAssertFalse(item.canRegenerate)
    }

    func testRecognitionErrorShowsExplanationInsteadOfRawCategory() throws {
        var wire = Self.wire(.dictation)
        wire.status = .transcriptionFailed
        wire.error.code = .recognitionFailed
        XCTAssertEqual(
            try HistoryItem(wire).message, "Aria couldn’t finish transcribing this recording.")
        wire.error.code = .UNRECOGNIZED(12345)
        XCTAssertEqual(
            try HistoryItem(wire).message, "Aria couldn’t finish transcribing this recording.")
        wire.error.code = .overallTimeout
        XCTAssertEqual(try HistoryItem(wire).message, "Aria couldn’t finish this recording in time.")
    }

    func testAudioExpiryMatchesAvailability() throws {
        var wire = Self.wire(.dictation)
        wire.status = .transcriptionFailed
        wire.audioAvailability = .available
        XCTAssertThrowsError(try HistoryItem(wire))
        wire.audioExpireTime = .init(seconds: 1)
        let expiry = Date(timeIntervalSince1970: 1)
        let available = try HistoryItem(wire)
        XCTAssertEqual(available.audio, .available(until: expiry))
        XCTAssertEqual(
            available.audioNote,
            "Its recording is kept until \(expiry.formatted(date: .abbreviated, time: .shortened)).")
        var successful = available
        successful.status = .success
        XCTAssertNil(successful.audioNote)
        let cases: [(Aria_Daemon_V1_HistoryAudioAvailability, HistoryItem.Audio, String)] = [
            (.unavailable, .unavailable, "Its recording wasn’t kept."),
            (.expired, .expired, "Its recording has expired."),
        ]
        for (audio, expected, description) in cases {
            wire.audioAvailability = audio
            XCTAssertThrowsError(try HistoryItem(wire))
            wire.clearAudioExpireTime()
            let item = try HistoryItem(wire)
            XCTAssertEqual(item.audio, expected)
            XCTAssertEqual(item.audioNote, description)
            wire.audioExpireTime = .init(seconds: 1)
        }
        wire.clearAudioExpireTime()
        for audio in [Aria_Daemon_V1_HistoryAudioAvailability.unspecified, .UNRECOGNIZED(12345)] {
            wire.audioAvailability = audio
            XCTAssertThrowsError(try HistoryItem(wire))
        }
    }
}
