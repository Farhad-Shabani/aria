import Foundation

/// A prompt run on a history entry from the palette, followed from its chat
/// events until it replies or fails.
struct PaletteResult: Equatable {
    enum State: Equatable {
        /// What the assistant is doing, such as `thinking`.
        case running(String)
        case replied(String)
        case failed(Failure)
    }

    struct Failure: Equatable {
        let message: String
        let canRetry: Bool

        init(_ message: String) {
            self.message = message
            canRetry = true
        }

        /// A missing prompt, or a missing, empty or unreadable entry, fails
        /// the same way each time.
        init(code: Aria_Daemon_V1_ErrorCode) {
            message = PaletteResult.message(for: code)
            canRetry = ![.promptMissing, .historyMissing, .historyEntryEmpty, .historyTranscriptUnreadable].contains(
                code)
        }
    }

    static let lost = "Aria lost track of this prompt. Try again."
    static let failed = "Aria couldn’t run this prompt. Try again."
    private static let earlyLimit = 64

    let promptName: String
    /// What the run asked of the assistant, which Try Again runs again. `nil` for a saved result,
    /// which can't be run again from here.
    let source: PromptSource?
    let input: HistoryItem
    private(set) var ticket: UInt64?
    private(set) var state = State.running("Starting…")
    /// Events that came before the ticket did. The call that returns the ticket can land after the
    /// run's first events.
    private var early: [Aria_Daemon_V1_ChatEvent] = []

    init(promptName: String, source: PromptSource, input: HistoryItem) {
        self.promptName = promptName
        self.source = source
        self.input = input
    }

    /// A reply saved in `input`'s thread, shown as a run that replied.
    init(saved result: PromptResult, on input: HistoryItem) {
        promptName = result.promptName
        source = nil
        self.input = input
        state = .replied(result.text)
    }

    var isRunning: Bool {
        if case .running = state { true } else { false }
    }

    mutating func started(ticket: UInt64) {
        self.ticket = ticket
        let held = early
        early = []
        for event in held { apply(event) }
    }

    mutating func fail(_ failure: Failure) {
        guard isRunning else { return }
        state = .failed(failure)
    }

    /// `nil` is the stream going away, and the run's reply with it.
    mutating func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        guard let event else {
            fail(Failure(Self.lost))
            return
        }
        guard let ticket else {
            early.append(event)
            if early.count > Self.earlyLimit { early.removeFirst() }
            return
        }
        guard event.ticket == ticket, isRunning else { return }
        switch event.stage {
        case .working:
            state = .running(Ask(ticket: ticket, step: event.step, skill: event.skill, since: .now).stepTitle)
        case .replied:
            state = .replied(event.text)
        case .failed:
            state = .failed(Failure(code: event.errorCode))
        default:
            break
        }
    }

    static func message(for code: Aria_Daemon_V1_ErrorCode) -> String {
        switch code {
        case .assistantSignedOut: "The assistant is signed out. Sign in, then try again."
        case .assistantUnavailable: "The assistant isn’t installed. Choose one in Settings."
        case .promptUnreadable: "Aria can’t read this prompt’s file. Check it in Settings."
        case .promptMissing: "This prompt was removed. Choose another one."
        case .cancelled: "The prompt was canceled."
        case .historyMissing: "This entry is no longer in history."
        case .historyEntryEmpty: "This entry has no text to run a prompt on."
        case .historyTranscriptUnreadable: "Aria can’t read this session’s whole transcript, so the prompt didn’t run."
        default: failed
        }
    }
}
