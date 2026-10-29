import Foundation

/// What a press asks the caller to do.
enum PressAction: Equatable {
    case record
    case ignore
}

/// Whether a press may start a run. The first one does; every press after
/// it is ignored until the run ends. The Aria key stops the recording and
/// cancels the run daemon-side, so no press here does.
///
/// The daemon holds the real state of a recording and an ask. This tracks
/// only whether a run is going, so the daemon never has to be asked.
struct PressGesture: Equatable {
    private enum Phase: Equatable {
        case idle
        /// From the first press until the ticket arrives.
        case recording
        case asking(ticket: UInt64)
    }

    private var phase = Phase.idle
    /// Asks that ended before their ticket reached the gesture. Every
    /// subscriber sees every ask, so a terminal's tickets land here too, and
    /// one slot would let a stranger's overwrite this one's.
    private var endedEarly: Set<UInt64> = []
    /// The stream dropped while recording, so the ask's end may pass
    /// unseen before the stream is back. Its ticket is not held.
    private var lostWhileRecording = false

    mutating func press() -> PressAction {
        guard phase == .idle else { return .ignore }
        phase = .recording
        return .record
    }

    /// Taken only before the ticket.
    mutating func issued(ticket: UInt64) {
        guard phase == .recording else { return }
        let ended = endedEarly.contains(ticket) || lostWhileRecording
        phase = ended ? .idle : .asking(ticket: ticket)
        endedEarly.removeAll()
        lostWhileRecording = false
    }

    /// An ask is over, however it ended. A stale ticket is ignored.
    mutating func ended(ticket: UInt64) {
        switch phase {
        case .asking(ticket):
            phase = .idle
        case .recording:
            endedEarly.insert(ticket)
        case .idle, .asking:
            break
        }
    }

    var isIdle: Bool { phase == .idle }

    /// The event stream dropped, so no reply or failure is coming for the
    /// ask in flight. Holding it would ignore every press for an ask the
    /// daemon has forgotten.
    mutating func streamLost() {
        switch phase {
        case .asking:
            abandon()
        case .recording:
            lostWhileRecording = true
        case .idle:
            break
        }
    }

    /// Nothing came of it — no speech, or a call that failed.
    mutating func abandon() {
        phase = .idle
        endedEarly.removeAll()
        lostWhileRecording = false
    }
}
