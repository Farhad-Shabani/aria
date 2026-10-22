import Foundation

struct CaptionState {
    private static let expiry: TimeInterval = 5
    private(set) var committed = ""
    private(set) var provisional = ""
    private var enabled = false
    private var recording: UInt64?
    private var ended = false
    private var updatedAt: TimeInterval?

    mutating func setEnabled(_ enabled: Bool) {
        guard self.enabled != enabled else { return }
        self.enabled = enabled
        clearText()
    }

    mutating func apply(_ snapshot: Aria_Daemon_V1_CaptionSnapshot?, now: TimeInterval) {
        guard let snapshot else {
            recording = nil
            ended = false
            clearText()
            return
        }
        if let recording, snapshot.recordingID < recording { return }
        if recording != snapshot.recordingID {
            recording = snapshot.recordingID
            ended = false
        }
        guard !ended else { return }
        ended = snapshot.ended
        guard enabled, !ended else {
            clearText()
            return
        }
        committed = String(String.UnicodeScalarView(snapshot.committed.unicodeScalars.suffix(2048)))
        provisional = String(String.UnicodeScalarView(snapshot.provisional.unicodeScalars.suffix(2048)))
        updatedAt = now
    }

    mutating func expire(now: TimeInterval) {
        guard let updatedAt, now - updatedAt >= Self.expiry else { return }
        clearText()
    }

    private mutating func clearText() {
        committed = ""
        provisional = ""
        updatedAt = nil
    }
}
