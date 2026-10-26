import AppKit
import Foundation

/// Runs an ask, or a stored prompt, from presses: one to record, the next to
/// send, the next to cancel. The presses come from a chord, the prompt
/// list, or both.
///
/// The daemon does the work and already announces every step, so this draws
/// nothing. The caption and status overlays show the recording and the
/// agent's progress from the same event stream.
@MainActor
final class PressController {
    enum Operation {
        case ask
        case prompt(id: String)
    }

    private let operation: Operation
    /// Hears the recording and ticket IDs, so the pill can name the run.
    private let started: @MainActor (RunID) -> Void
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    private var gesture = PressGesture()
    /// The app in front at the first press: a prompt's result is typed only
    /// there.
    private var pressedIn: String?
    /// The open `Start`, kept so a stop that never lands can end it.
    private var recording: Task<Void, Never>?

    /// An ask ends on the event stream, not on the call that started it: the
    /// third press cancels, but a reply or a failure ends it just as well.
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
        act(on: gesture.press())
    }

    var isBusy: Bool { !gesture.isIdle }

    /// A stop that finds nothing running is not chased: the open `Start`
    /// answers on its own, with a result, a stop-accepted, or nothing, and
    /// `run` reads all three. A stop that never reached the daemon is
    /// different: the mic would stay live until the daemon's own deadline,
    /// so the open `Start` is cancelled, which ends the press daemon-side.
    private func act(on action: PressAction) {
        switch action {
        case .record:
            pressedIn = NSWorkspace.shared.frontmostApplication?.bundleIdentifier
            recording = Task { await self.run() }
        case .stop(let id):
            Task {
                if await PressClient.stopRecording(id: id) == nil {
                    recording?.cancel()
                    gesture.abandon()
                }
            }
        case .cancel(let ticket):
            Task { await PressClient.cancel(ticket: ticket) }
        case .ignore:
            break
        }
    }

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
            case .prompt(let id): await runPrompt(id: id)
            }
        guard let ticket else {
            gesture.abandon()
            return
        }
        started(.ticket(ticket))
        gesture.issued(ticket: ticket)
    }

    private func ask() async -> UInt64? {
        guard let target = await PressClient.resolve(), !Task.isCancelled,
            let heard = await record(projectDirectory: target.workingDirectory)
        else { return nil }
        return await PressClient.ask(heard: heard, target: target)
    }

    /// A prompt names no project, so the daemon picks the vocabulary.
    private func runPrompt(id: String) async -> UInt64? {
        guard let heard = await record(projectDirectory: nil) else { return nil }
        return await PressClient.runPrompt(text: heard.text, promptID: id, application: pressedIn)
    }

    /// Nil for silence as well as for a recording that failed.
    private func record(projectDirectory: String?) async -> Aria_Daemon_V1_DictationResult? {
        let heard = await PressClient.transcribe(projectDirectory: projectDirectory) { [weak self] id in
            guard let self else { return }
            started(.recording(id))
            act(on: gesture.recordingStarted(id: id))
        }
        guard let heard, !heard.text.isEmpty else { return nil }
        return heard
    }
}
