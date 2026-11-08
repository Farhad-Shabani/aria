import Foundation

/// One chat the daemon has not answered yet.
struct PendingChat: Equatable {
    let ticket: UInt64
    var step: Aria_Daemon_V1_ChatStep
    var skill: String
    let since: Date

    var stepLabel: String {
        step == .skill && !skill.isEmpty ? "using \(skill)" : step.label
    }

    /// The step as a page shows it, in sentence case: “Thinking…”. The pill keeps it lowercase.
    var stepTitle: String {
        stepLabel.prefix(1).uppercased() + stepLabel.dropFirst() + "…"
    }
}

/// Which chats the daemon has not answered yet, by ticket.
struct ChatsInFlight: Equatable {
    private var chats: [UInt64: PendingChat] = [:]

    var count: Int { chats.count }
    var oldest: PendingChat? { chats.min(by: { $0.key < $1.key })?.value }

    /// Drops a chat that something else shows from here on.
    mutating func forget(_ ticket: UInt64) {
        chats[ticket] = nil
    }

    /// `nil` is the stream going away, and every chat with it.
    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?, now: Date = Date()) {
        guard let event else {
            chats = [:]
            return
        }
        switch event.stage {
        case .working:
            var chat =
                chats[event.ticket]
                ?? PendingChat(ticket: event.ticket, step: .thinking, skill: "", since: now)
            chat.step = event.step
            chat.skill = event.skill
            chats[event.ticket] = chat
        case .replied, .failed: chats[event.ticket] = nil
        default: break
        }
    }
}

extension Aria_Daemon_V1_ChatStep {
    fileprivate var label: String {
        switch self {
        case .thinking: return "thinking"
        case .reading: return "reading"
        case .searching: return "searching"
        case .running: return "running"
        case .skill: return "using a skill"
        case .unspecified, .UNRECOGNIZED: return "working"
        }
    }
}
