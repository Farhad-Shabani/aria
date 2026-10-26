import Foundation

/// The enrolled voice heard in the press on screen. Each press starts
/// unheard, even one that fails before it records. An event naming no voice
/// keeps the last, so the ring does not drop out mid-press.
struct HeardVoice {
    private(set) var name: String?
    private var hasEnded = false

    mutating func apply(_ event: Aria_Daemon_V1_DictationEvent) {
        if hasEnded {
            name = nil
            hasEnded = false
        }
        switch event.stage {
        case .recording: name = nil
        case .segmentTranscribed where !event.speakerName.isEmpty: name = event.speakerName
        case .delivered, .cancelled, .failed: hasEnded = true
        default: break
        }
    }
}
