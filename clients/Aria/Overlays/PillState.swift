import AppKit
import SwiftUI

enum PillState: Equatable {
    case recording(since: Date)
    case transcribing
    case delivering
    /// The press delivered: where, and how many words.
    case delivered(DeliveryOutcome, words: Int)
    case cancelled
    /// Focus moved, so typing stopped. Not a failure.
    case interrupted
    /// The app in front has Aria switched off, by name when known.
    case excluded(String?)
    /// A press that failed, in the words the person needs.
    case failed(String)
    /// An ask in flight: its step, such as `reading · +1`, and since when.
    case asking(String, since: Date)
    case replied(copied: Bool)
    case noReply

    init?(
        _ event: Aria_Daemon_V1_DictationEvent?,
        now: Date = Date(),
        frontApp: @autoclosure () -> String? = NSWorkspace.shared.frontmostApplication?.localizedName
    ) {
        guard let event else { return nil }
        switch event.stage {
        case .recording: self = .recording(since: now)
        case .transcribing: self = .transcribing
        case .delivering: self = .delivering
        case .delivered: self = .delivered(DeliveryOutcome(event.outcome), words: Int(event.wordCount))
        case .cancelled: self = .cancelled
        case .failed where [.deliveryTargetChanged, .deliveryInterrupted].contains(event.errorCode):
            self = .interrupted
        case .failed where event.errorCode == .applicationExcluded: self = .excluded(frontApp())
        case .failed: self = .failed(event.errorCode.dictationMessage)
        // A segment lands while the speaker is still talking, so it says
        // nothing about the press.
        case .segmentTranscribed, .segmentDelivered: return nil
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    /// The pill for an ask that just ended, or nothing for one under way.
    init?(ended event: Aria_Daemon_V1_ChatEvent) {
        switch event.stage {
        case .replied: self = .replied(copied: event.copied)
        case .failed: self = .noReply
        default: return nil
        }
    }

    /// The pill for the asks in flight: the oldest one's step, and how many
    /// others are in flight.
    init?(asks: AsksInFlight) {
        guard let oldest = asks.oldest else { return nil }
        var step = oldest.stepLabel
        if asks.count > 1 {
            step += " · +\(asks.count - 1)"
        }
        self = .asking(step, since: oldest.since)
    }

    /// How long the pill stays once the press is over. A live press has no
    /// life of its own: the next event ends it.
    var life: Duration? {
        switch self {
        case .recording, .transcribing, .delivering, .asking: return nil
        case .delivered, .replied: return .milliseconds(1200)
        case .cancelled: return .milliseconds(800)
        case .interrupted, .excluded: return .seconds(2)
        // Long enough to read, and the next press clears it anyway.
        case .failed, .noReply: return .seconds(5)
        }
    }

    var tint: Color {
        switch self {
        case .recording, .cancelled, .failed, .noReply:
            return Color(red: 0.88, green: 0.42, blue: 0.45)
        case .transcribing, .delivering, .asking:
            return Color(red: 0.90, green: 0.75, blue: 0.48)
        case .delivered, .replied:
            return Color(red: 0.48, green: 0.80, blue: 0.59)
        case .interrupted, .excluded:
            return Color(white: 0.72)
        }
    }

    var motion: Motion {
        switch self {
        case .asking, .delivering: return .breathe
        case .recording, .transcribing, .delivered, .cancelled, .interrupted, .excluded, .failed, .replied,
            .noReply:
            return .still
        }
    }

    /// The words beside the mark, or none where the mark says it all. A
    /// prompt's name leads its own run, so a wrong chord shows before
    /// anything is said.
    func label(prompt: String? = nil) -> String? {
        let words = [prompt.map(Self.shortened), detail].compactMap { $0 }
        return words.isEmpty ? nil : words.joined(separator: " · ")
    }

    /// A name short enough to leave room in the pill for the step and the
    /// clock.
    private static func shortened(_ name: String) -> String {
        let length = 20
        guard name.count > length else { return name }
        return name.prefix(length - 1).trimmingCharacters(in: .whitespaces) + "…"
    }

    private var detail: String? {
        switch self {
        case .recording: return nil
        case .transcribing: return "transcribing"
        case .delivering: return "typing"
        case .delivered(let outcome, let words):
            let count = words == 1 ? "1 word" : "\(words) words"
            return outcome == .copied ? "copied · \(count)" : count
        case .cancelled: return "cancelled"
        case .interrupted: return "stopped · focus moved"
        case .excluded(let app): return "off in \(app ?? "this app")"
        case .failed(let message): return message
        case .asking(let step, _): return step
        case .replied(let copied): return copied ? "copied" : "replied"
        case .noReply: return "no reply"
        }
    }

    /// The clock the pill runs, for the states worth timing.
    var since: Date? {
        switch self {
        case .recording(let since), .asking(_, let since): return since
        default: return nil
        }
    }
}

/// Where a delivered transcript went. Mirrors the daemon's own
/// `DeliveryOutcome`.
enum DeliveryOutcome: Equatable {
    case inserted
    case copied
    /// A press that asked only for its transcript, or a daemon too old to say.
    case unknown

    init(_ code: Aria_Daemon_V1_DeliveryOutcome) {
        switch code {
        case .inserted: self = .inserted
        case .copied: self = .copied
        case .unspecified, .UNRECOGNIZED: self = .unknown
        }
    }

}

extension Aria_Daemon_V1_ErrorCode {
    /// What a failed press says. The daemon logs a code; a person reads this.
    fileprivate var dictationMessage: String {
        switch self {
        case .noSpeech: return "No speech detected"
        case .captureFailed: return "Recording failed — check your mic"
        case .transcriptionFailed: return "Transcription failed"
        case .deliveryFailed, .deliveryTimeout: return "The transcript could not be delivered"
        case .recordingTimeout, .overallTimeout: return "The press ran past its time limit"
        default: return "Dictation failed"
        }
    }
}

enum Motion: Equatable {
    case breathe
    case still

    var scale: CGFloat {
        switch self {
        case .breathe: return 1.25
        case .still: return 1
        }
    }

    var animation: Animation? {
        switch self {
        case .breathe: return .easeInOut(duration: 1.4).repeatForever()
        case .still: return nil
        }
    }
}
