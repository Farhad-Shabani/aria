import Foundation

/// One ask the daemon has not answered yet.
struct Ask: Equatable {
    let ticket: UInt64
    var step: Aria_Daemon_V1_ChatStep
    var skill: String
    let since: Date

    var stepLabel: String {
        step == .skill ? "skill: \(skill)" : step.label
    }
}

/// Which asks the daemon has not answered yet, by ticket.
struct AsksInFlight: Equatable {
    private var asks: [UInt64: Ask] = [:]

    var count: Int { asks.count }
    var oldest: Ask? { asks.min(by: { $0.key < $1.key })?.value }

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
                ?? Ask(ticket: event.ticket, step: .thinking, skill: "", since: now)
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
        case .skill: return "skill"
        case .unspecified, .UNRECOGNIZED: return "unknown"
        }
    }
}
