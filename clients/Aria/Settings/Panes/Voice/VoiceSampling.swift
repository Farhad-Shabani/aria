/// How far a voice recording has come, as the daemon reports it.
struct VoiceSampling: Equatable {
    var taken = 0
    /// Whether audio flows for the next sample, so the person should read now.
    var isListening = false

    mutating func hear(_ event: Aria_Daemon_V1_SpeakerEvent) {
        switch event.stage {
        case .recording: self = VoiceSampling(taken: Int(event.index) - 1, isListening: true)
        case .sampleTaken: self = VoiceSampling(taken: Int(event.index), isListening: false)
        case .unspecified, .enrolled, .cancelled, .failed, .UNRECOGNIZED: break
        }
    }
}
