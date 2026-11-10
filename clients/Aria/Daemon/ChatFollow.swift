/// Picks one chat run's events out of the stream by its ticket. The call that returns the ticket
/// can land after the run's first events, so those events are held until the ticket comes.
struct ChatFollow: Equatable {
    private static let earlyLimit = 64

    private(set) var ticket: UInt64?
    private var early: [Aria_Daemon_V1_ChatEvent] = []

    mutating func started(ticket: UInt64) -> [Aria_Daemon_V1_ChatEvent] {
        self.ticket = ticket
        let held = early.filter { $0.ticket == ticket }
        early = []
        return held
    }

    /// Holds an event that comes before the ticket for `started`.
    mutating func admits(_ event: Aria_Daemon_V1_ChatEvent) -> Bool {
        guard let ticket else {
            early.append(event)
            if early.count > Self.earlyLimit { early.removeFirst() }
            return false
        }
        return event.ticket == ticket
    }
}
