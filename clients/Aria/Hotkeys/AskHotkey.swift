import AppKit
import Carbon.HIToolbox

/// The chord that runs an ask, as the daemon stores it.
///
/// Modifiers arrive named rather than as a mask: the preference file is
/// shared with platforms that have no Carbon constants.
struct AskHotkey: Equatable {
    let enabled: Bool
    let keyCode: UInt16
    let control: Bool
    let option: Bool
    let shift: Bool
    let command: Bool

    var carbonModifiers: UInt32 {
        var mask: UInt32 = 0
        if control { mask |= UInt32(controlKey) }
        if option { mask |= UInt32(optionKey) }
        if shift { mask |= UInt32(shiftKey) }
        if command { mask |= UInt32(cmdKey) }
        return mask
    }

    /// A chord with no modifier takes a bare key away from every app, so the
    /// daemon refuses one.
    var hasModifier: Bool {
        control || option || shift || command
    }

    /// Command alone is where an app keeps its own menu keys — ⌘Q, ⌘W, ⌘S.
    /// A registered chord is consulted before them, everywhere.
    var isCommandOnly: Bool {
        command && !(control || option || shift)
    }

    var wire: Aria_Daemon_V1_AskHotkey {
        var message = Aria_Daemon_V1_AskHotkey()
        message.enabled = enabled
        message.keyCode = UInt32(keyCode)
        message.control = control
        message.option = option
        message.shift = shift
        message.command = command
        return message
    }

    func with(enabled: Bool) -> AskHotkey {
        AskHotkey(
            enabled: enabled,
            keyCode: keyCode,
            control: control,
            option: option,
            shift: shift,
            command: command
        )
    }
}

extension AskHotkey {
    /// Nil for a key code no keyboard can produce. The daemon keeps this as
    /// a `u16` and refuses anything wider, so a wider one means a client
    /// this build should not follow.
    init?(wire: Aria_Daemon_V1_AskHotkey) {
        guard let keyCode = UInt16(exactly: wire.keyCode) else { return nil }
        self.init(
            enabled: wire.enabled,
            keyCode: keyCode,
            control: wire.control,
            option: wire.option,
            shift: wire.shift,
            command: wire.command
        )
    }

    /// What one keypress during capture means.
    enum Capture: Equatable {
        case cancelled
        case refused(String)
        case chord(AskHotkey)
    }

    /// Reads a keypress as a chord, so the rules hold without a window open.
    static func captured(keyCode: UInt16, modifiers: NSEvent.ModifierFlags) -> Capture {
        guard keyCode != UInt16(kVK_Escape) else { return .cancelled }
        let chord = AskHotkey(
            enabled: true,
            keyCode: keyCode,
            control: modifiers.contains(.control),
            option: modifiers.contains(.option),
            shift: modifiers.contains(.shift),
            command: modifiers.contains(.command)
        )
        guard chord.hasModifier else {
            return .refused("Hold Control, Option, Shift, or Command as well.")
        }
        guard !chord.isCommandOnly else {
            return .refused("Command alone belongs to the app in front. Add another modifier.")
        }
        return .chord(chord)
    }
}

/// What a press asks the caller to do.
enum AskAction: Equatable {
    case record
    case stop(recording: UInt64)
    case cancel(ticket: UInt64)
    case ignore
}

/// Where one chord's three presses stand.
///
/// The daemon holds the real state of a recording and an ask. This tracks
/// only what the next press means, so the daemon never has to be asked.
struct AskGesture: Equatable {
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
    mutating func press() -> AskAction {
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
    mutating func recordingStarted(id: UInt64) -> AskAction {
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
        guard beforeTicket else { return }
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

    private var beforeTicket: Bool {
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
