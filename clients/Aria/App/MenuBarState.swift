/// The daemon's state outranks the pill's.
enum MenuBarState: Equatable {
    case ready
    case listening
    case working
    case done
    /// The daemon starting or stopping.
    case switching
    case stopped
    case attention

    init(daemon: DaemonState, pill: PillState?) {
        switch (daemon, pill) {
        case (.stopped, _): self = .stopped
        case (.working, _): self = .switching
        case (.running, .recording): self = .listening
        case (.running, .transcribing), (.running, .delivering), (.running, .asking): self = .working
        case (.running, .delivered), (.running, .replied): self = .done
        case (.running, .failed), (.running, .noReply): self = .attention
        case (.running, .cancelled), (.running, nil): self = .ready
        }
    }

    var accessibilityLabel: String {
        switch self {
        case .ready: return "Aria"
        case .listening: return "Aria, listening"
        case .working: return "Aria, working"
        case .done: return "Aria, done"
        case .switching: return "Aria, starting or stopping"
        case .stopped: return "Aria, stopped"
        case .attention: return "Aria, needs attention"
        }
    }
}
