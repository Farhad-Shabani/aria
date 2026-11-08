import AppKit
import SwiftUI

enum PillState: Equatable {
    /// Recording, and once a segment queues, the app its queued words wait
    /// for, until one types.
    case recording(since: Date, waitingFor: StartApp? = nil)
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
    /// A chat in flight: its step, such as `reading · +1`, and since when.
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
        // A segment or a voice arrives while the speaker is still talking,
        // so it says nothing about the press.
        case .segmentTranscribed, .segmentDelivered, .segmentQueued, .speakerChanged: return nil
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    /// After a prompt's name, before or after its recording.
    static let unreadablePrompt = "file unavailable"

    /// A prompt turned down before its recording, named as its run would be.
    static func refused(prompt name: String) -> PillState {
        .failed([shortened(name), unreadablePrompt].joined(separator: " · "))
    }

    /// The pill for a chat that just ended, or nothing for one under way.
    init?(ended event: Aria_Daemon_V1_ChatEvent) {
        switch event.stage {
        case .replied: self = .replied(copied: event.copied)
        case .failed: self = event.errorCode.chatMessage.map(PillState.failed) ?? .noReply
        default: return nil
        }
    }

    /// The pill for the chats in flight: the oldest one's step, and how many
    /// others are in flight.
    init?(chats: ChatsInFlight) {
        guard let oldest = chats.oldest else { return nil }
        var step = oldest.stepLabel
        if chats.count > 1 {
            step += " · +\(chats.count - 1)"
        }
        self = .asking(step, since: oldest.since)
    }

    /// The live press after a streamed segment: queued words wait for
    /// `startApp`, typed words end the wait. Nil when the pill shows no
    /// recording.
    func segment(_ stage: Aria_Daemon_V1_DictationStage, startApp: StartApp) -> PillState? {
        guard case .recording(let since, _) = self else { return nil }
        switch stage {
        case .segmentQueued: return .recording(since: since, waitingFor: startApp)
        case .segmentDelivered: return .recording(since: since)
        default: return nil
        }
    }

    /// How long the pill stays once the press is over. A live press has no
    /// life of its own: the next event ends it.
    var life: Duration? {
        switch self {
        case .recording, .transcribing, .delivering, .asking: return nil
        // Words that missed the field leave the user something to do, so
        // their notice stays to be read.
        case .delivered(.copied, _), .delivered(.inHistory, _), .delivered(.notTyped, _),
            .replied(copied: true):
            return .seconds(4)
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
            return OverlayStyle.alertTint
        case .transcribing, .delivering, .asking:
            return OverlayStyle.workingTint
        // Words that never reached the field get no success color.
        case .delivered(.inHistory, _), .delivered(.notTyped, _), .interrupted, .excluded:
            return OverlayStyle.quietTint
        case .delivered, .replied:
            return OverlayStyle.doneTint
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
        // The start app's icon says it all; without one, a word does.
        case .recording(_, let startApp?): return startApp.app == nil ? "waiting" : nil
        case .recording: return nil
        case .transcribing: return "transcribing"
        case .delivering: return "typing"
        case .delivered(let outcome, let words):
            let count = words == 1 ? "1 word" : "\(words) words"
            switch outcome {
            case .copied: return "copied · \(count)"
            case .inHistory: return "saved to history · \(count)"
            case .notTyped: return "not typed · \(count)"
            case .inserted, .unknown: return count
            }
        case .cancelled: return "canceled"
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
        case .recording(let since, _), .asking(_, let since): return since
        default: return nil
        }
    }
}

/// The app a press started in, which its queued words wait for. An app
/// without a bundle ID has no icon to show, so the pill says "waiting".
struct StartApp: Equatable {
    let app: InstalledApp?
}

extension StartApp {
    /// Looks the app up as the press starts, so drawing the pill reads no
    /// files.
    @MainActor
    init(bundleID: String?) {
        self.init(app: bundleID.map(InstalledApp.lookUp))
    }
}

/// Where delivered text went. Mirrors the daemon's own
/// `DeliveryOutcome`.
enum DeliveryOutcome: Equatable {
    case inserted
    case copied
    /// Never typed: the press ended away from its field. History keeps it.
    case inHistory
    /// Never typed, and history saving is off.
    case notTyped
    /// A press that asked only for its text, or a daemon too old to say.
    case unknown

    init(_ code: Aria_Daemon_V1_DeliveryOutcome) {
        switch code {
        case .inserted: self = .inserted
        case .copied: self = .copied
        case .inHistory: self = .inHistory
        case .notTyped: self = .notTyped
        case .unspecified, .UNRECOGNIZED: self = .unknown
        }
    }

}

extension Aria_Daemon_V1_ErrorCode {
    /// What a failed press says. The daemon logs a code; a person reads this.
    fileprivate var dictationMessage: String {
        switch self {
        case .noSpeech: return "no speech"
        case .captureFailed: return "couldn’t record"
        case .recognitionFailed: return "couldn’t transcribe"
        case .deliveryFailed, .deliveryTimeout: return "couldn’t type"
        case .recordingTimeout, .overallTimeout: return "timed out"
        default: return "couldn’t dictate"
        }
    }

    /// What a failed chat says when the person can fix the cause.
    fileprivate var chatMessage: String? {
        switch self {
        case .assistantSignedOut: return "assistant signed out"
        case .assistantUnavailable: return "assistant not installed"
        case .promptUnreadable: return PillState.unreadablePrompt
        default: return nil
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
