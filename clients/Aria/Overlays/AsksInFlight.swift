import Foundation

/// One ask the daemon has not answered yet.
struct PendingAsk: Equatable {
    let ticket: UInt64
    var step: Aria_Daemon_V1_ChatStep
    var skill: String
    let since: Date

    var stepLabel: String { Self.label(of: step, skill: skill) }

    /// A step as a page shows it, in sentence case: “Thinking…”. The pill keeps it lowercase.
    static func title(of step: Aria_Daemon_V1_ChatStep, skill: String) -> String {
        let text = label(of: step, skill: skill)
        return text.prefix(1).uppercased() + text.dropFirst() + "…"
    }

    private static func label(of step: Aria_Daemon_V1_ChatStep, skill: String) -> String {
        step == .skill && !skill.isEmpty ? "using \(skill)" : step.label
    }
}

/// Which asks the daemon has not answered yet, by ticket.
struct AsksInFlight: Equatable {
    private var asks: [UInt64: PendingAsk] = [:]

    var count: Int { asks.count }
    var oldest: PendingAsk? { asks.min(by: { $0.key < $1.key })?.value }

    /// Drops an ask that something else shows from here on.
    mutating func forget(_ ticket: UInt64) {
        asks[ticket] = nil
    }

    /// `nil` is the stream going away, and every ask with it.
    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?, now: Date = Date()) {
        guard let event else {
            asks = [:]
            return
        }
        switch event.stage {
        case .working:
            var ask =
                asks[event.ticket]
                ?? PendingAsk(ticket: event.ticket, step: .thinking, skill: "", since: now)
            ask.step = event.step
            ask.skill = event.skill
            asks[event.ticket] = ask
        case .replied, .failed: asks[event.ticket] = nil
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
