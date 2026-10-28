import Foundation

/// The current speaker in the press on screen. Each press starts
/// unheard, even one that fails before it records.
struct HeardSpeaker {
    enum Voice: Equatable {
        /// An enrolled voice whose words are typed.
        case named(String)
        /// A voice whose words are not typed: the press types only the owner's.
        case keptOut
    }

    /// `nil` while the voice is unrecognized or not judged yet.
    private(set) var voice: Voice?
    private var hasEnded = false

    mutating func apply(_ event: Aria_Daemon_V1_DictationEvent?) {
        guard let event else {
            self = Self()
            return
        }
        if hasEnded {
            voice = nil
            hasEnded = false
        }
        switch event.stage {
        case .recording: voice = nil
        case .speakerChanged: voice = Self.voice(of: event)
        case .delivered, .cancelled, .failed: hasEnded = true
        default: break
        }
    }

    private static func voice(of event: Aria_Daemon_V1_DictationEvent) -> Voice? {
        switch event.heard {
        case .typed: event.speakerName.isEmpty ? nil : .named(event.speakerName)
        case .keptOut: .keptOut
        case .undecided, .unspecified, .UNRECOGNIZED: nil
        }
    }
}
