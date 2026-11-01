/// The daemon's state outranks the pill's.
enum MenuBarState: Equatable {
    case ready
    case listening
    case transcribingSystemAudio
    case working
    case done
    /// The daemon starting or stopping.
    case switching
    case stopped
    case attention
    case updateAvailable

    init(daemon: DaemonState, pill: PillState?, isListenRunning: Bool, isUpdateAvailable: Bool) {
        switch (daemon, pill) {
        case (.stopped, _): self = .stopped
        case (.working, _): self = .switching
        case (.running, .recording): self = .listening
        case (.running, .transcribing), (.running, .delivering), (.running, .asking): self = .working
        case (.running, .delivered), (.running, .replied): self = .done
        case (.running, .failed), (.running, .noReply): self = .attention
        case (.running, .cancelled), (.running, .interrupted), (.running, .excluded), (.running, nil):
            if isListenRunning {
                self = .transcribingSystemAudio
            } else {
                self = isUpdateAvailable ? .updateAvailable : .ready
            }
        }
    }

    var accessibilityLabel: String {
        switch self {
        case .ready: return "Aria"
        case .listening: return "Aria, listening"
        case .transcribingSystemAudio: return "Aria, transcribing your Mac’s audio"
        case .working: return "Aria, working"
        case .done: return "Aria, done"
        case .switching: return "Aria, starting or stopping"
        case .stopped: return "Aria, stopped"
        case .attention: return "Aria, needs attention"
        case .updateAvailable: return "Aria, update available"
        }
    }
}
