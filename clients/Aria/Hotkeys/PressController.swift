import AppKit
import Foundation

/// Runs an ask, or a stored prompt, from a press: a chord, an Aria key
/// combo, or the prompt list. A press only starts the run. The Aria key
/// then stops its recording and cancels it, daemon-side.
///
/// The daemon does the work and already announces every step, so this draws
/// nothing. The caption and status overlays show the recording and the
/// agent's progress from the same event stream.
@MainActor
final class PressController {
    enum Operation {
        case ask
        /// `canRun` is asked before recording, so a prompt that can't run
        /// records nothing.
        case prompt(id: String, canRun: @MainActor () -> Bool)
    }

    private let operation: Operation
    /// Hears the recording and ticket IDs, so the pill can name the run.
    private let started: @MainActor (RunID) -> Void
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    private var gesture = PressGesture()
    /// The app in front at the first press: a prompt's result is typed only
    /// there.
    private var pressedIn: String?

    /// An ask ends on the event stream, not on the call that started it: a
    /// reply, a failure, or the daemon's own cancel.
    init(
        events: DaemonEvents,
        operation: Operation,
        started: @escaping @MainActor (RunID) -> Void = { _ in }
    ) {
        self.operation = operation
        self.started = started
        events.onChat { [weak self] chat in
            guard let self else { return }
            guard let chat else {
                gesture.streamLost()
                return
            }
            switch chat.stage {
            case .replied, .failed:
                gesture.ended(ticket: chat.ticket)
            default:
                break
            }
        }
    }

    func press() {
        switch gesture.press() {
        case .record:
            pressedIn = NSWorkspace.shared.frontmostApplication?.bundleIdentifier
            Task { await self.run() }
        case .ignore:
            break
        }
    }

    var isBusy: Bool { !gesture.isIdle }

    /// One `Start` spans the whole recording and answers with the transcript
    /// once it has been stopped. Nothing is typed at that point: the reply
    /// is what gets delivered, not what was said.
    ///
    /// Silence just returns to idle. A chord has no way to explain itself,
    /// and the daemon's own failure sound already covers a broken recording.
    private func run() async {
        let ticket =
            switch operation {
            case .ask: await ask()
            case .prompt(let id, let canRun): await runPrompt(id: id, canRun: canRun)
            }
        guard let ticket else {
            gesture.abandon()
            return
        }
        started(.ticket(ticket))
        gesture.issued(ticket: ticket)
    }

    private func ask() async -> UInt64? {
        guard let target = await PressClient.resolve(),
            let heard = await record(projectDirectory: target.workdir)
        else { return nil }
        return await PressClient.ask(heard: heard, target: target)
    }

    /// A prompt names no project, so the daemon picks the vocabulary.
    private func runPrompt(id: String, canRun: @MainActor () -> Bool) async -> UInt64? {
        guard canRun(), let heard = await record(projectDirectory: nil) else { return nil }
        return await PressClient.runPrompt(text: heard.text, promptID: id, application: pressedIn)
    }

    /// Nil for silence as well as for a recording that failed.
    private func record(projectDirectory: String?) async -> Aria_Daemon_V1_DictationResult? {
        let heard = await PressClient.transcribe(projectDirectory: projectDirectory) { [weak self] id in
            self?.started(.recording(id))
        }
        guard let heard, !heard.text.isEmpty else { return nil }
        return heard
    }
}
