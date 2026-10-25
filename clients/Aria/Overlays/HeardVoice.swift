import Foundation

/// The enrolled voice heard in the press on screen. Each press starts
/// unheard, even one that fails before it records. An event naming no voice
/// keeps the last, so the ring does not drop out mid-press.
struct HeardVoice {
    private(set) var name: String?
    private var ended = false

    mutating func apply(_ event: Aria_Daemon_V1_DictationEvent) {
        if ended {
            name = nil
            ended = false
        }
        switch event.stage {
        case .recording: name = nil
        case .segmentTranscribed where !event.speaker.isEmpty: name = event.speaker
        case .delivered, .cancelled, .failed: ended = true
        default: break
        }
    }
}
