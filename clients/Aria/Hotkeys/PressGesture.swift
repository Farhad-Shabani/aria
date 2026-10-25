import Foundation

/// What a press asks the caller to do.
enum PressAction: Equatable {
    case record
    case stop(recording: UInt64)
    case cancel(ticket: UInt64)
    case ignore
}

/// Where one chord's three presses stand.
///
/// The daemon holds the real state of a recording and an ask. This tracks
/// only what the next press means, so the daemon never has to be asked.
struct PressGesture: Equatable {
    private enum Phase: Equatable {
        case idle
        /// The daemon names the recording as it opens; until then a press
        /// has nothing to stop.
        case recording(id: UInt64?)
        /// A press landed before the recording was named. The stop goes
        /// out the moment the name arrives.
        case stopRequested
        /// The recording is ending, or the ask is accepted but its ticket
        /// has not arrived.
        case sending
        case asking(ticket: UInt64)
    }

    private var phase = Phase.idle
    /// Asks that ended before their ticket reached the gesture. Every
    /// subscriber sees every ask, so a terminal's tickets land here too, and
    /// one slot would let a stranger's overwrite this one's.
    private var endedEarly: Set<UInt64> = []

    /// A press while sending is ignored: the gap is short, and a cancel
    /// queued there would kill the ask the user just started.
    mutating func press() -> PressAction {
        switch phase {
        case .idle:
            phase = .recording(id: nil)
            return .record
        case .recording(let id?):
            phase = .sending
            return .stop(recording: id)
        case .recording(nil):
            phase = .stopRequested
            return .ignore
        case .stopRequested, .sending:
            return .ignore
        case .asking(let ticket):
            phase = .idle
            return .cancel(ticket: ticket)
        }
    }

    /// The daemon has named the recording. A press that was waiting on the
    /// name is spent now.
    mutating func recordingStarted(id: UInt64) -> PressAction {
        switch phase {
        case .recording(nil):
            phase = .recording(id: id)
            return .ignore
        case .stopRequested:
            phase = .sending
            return .stop(recording: id)
        default:
            return .ignore
        }
    }

    /// Taken from any phase before the ticket. Another client's start ends
    /// a manual recording, so a transcript can arrive with no second press
    /// behind it.
    mutating func issued(ticket: UInt64) {
        guard isBeforeTicket else { return }
        phase = endedEarly.contains(ticket) ? .idle : .asking(ticket: ticket)
        endedEarly.removeAll()
    }

    /// An ask is over, however it ended. A stale ticket is ignored: the
    /// press that cancelled it already moved on.
    mutating func ended(ticket: UInt64) {
        switch phase {
        case .asking(ticket):
            phase = .idle
        case .recording, .stopRequested, .sending:
            endedEarly.insert(ticket)
        default:
            break
        }
    }

    var isIdle: Bool { phase == .idle }

    private var isBeforeTicket: Bool {
        switch phase {
        case .recording, .stopRequested, .sending: true
        case .idle, .asking: false
        }
    }

    /// The event stream dropped, so no reply or failure is coming for the
    /// ask in flight. Holding it would spend the next press cancelling a
    /// ticket the daemon has forgotten — or worse, one it has since reused.
    mutating func streamLost() {
        if case .asking = phase {
            abandon()
        }
    }

    /// Nothing came of it — no speech, or a call that failed.
    mutating func abandon() {
        phase = .idle
        endedEarly.removeAll()
    }
}
