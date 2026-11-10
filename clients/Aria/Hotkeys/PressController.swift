import AppKit
import Foundation

/// Runs an ask, or a stored prompt, from a press: a chord, an Aria key
/// combo, or the prompt list. A press only starts the run. The Aria key
/// then stops its recording and cancels it, daemon-side.
@MainActor
final class PressController {
    enum Operation {
        /// `target` is read as the key is pressed, and `nil` ignores the press. `show` runs once
        /// the question is heard.
        case ask(Chats, target: @MainActor () -> AskTarget?, show: @MainActor () -> Void)
        /// `canRun` is asked before recording, so a prompt that can't run
        /// records nothing.
        case prompt(id: String, canRun: @MainActor () -> Bool)
    }

    struct Effects {
        /// Records until the press is stopped. `nil` for silence and for a recording that failed.
        var transcribe:
            (_ projectDirectory: String?, _ onRecording: @escaping @MainActor (UInt64) -> Void) async ->
                DictationClient.Transcript?
        var resolve: () async -> ChatTarget?
        var runPrompt: (_ text: String, _ promptID: String, _ application: String?) async -> UInt64?

        static var live: Self {
            .init(
                transcribe: PressClient.transcribe,
                resolve: PressClient.resolve,
                runPrompt: PressClient.runPrompt)
        }
    }

    private enum Run {
        case ask(AskTarget, Chats, show: @MainActor () -> Void)
        case prompt(id: String, canRun: @MainActor () -> Bool)
    }

    private let operation: Operation
    private let effects: Effects
    /// Hears the recording and ticket IDs, so the pill can name the run.
    private let started: @MainActor (RunID) -> Void
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    private var gesture = PressGesture()
    /// The app in front at the first press: a prompt's result is typed only
    /// there.
    private var pressedIn: String?

    /// A chat ends on the event stream, not on the call that started it: a
    /// reply, a failure, or the daemon's own cancel.
    init(
        events: DaemonEvents,
        operation: Operation,
        effects: Effects = .live,
        started: @escaping @MainActor (RunID) -> Void = { _ in }
    ) {
        self.operation = operation
        self.effects = effects
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
        // The target is read at press time, since the chat on screen then is the one the person asks into.
        guard let run = pressed() else { return }
        switch gesture.press() {
        case .record:
            pressedIn = NSWorkspace.shared.frontmostApplication?.bundleIdentifier
            Task { await self.run(run) }
        case .ignore:
            break
        }
    }

    private func pressed() -> Run? {
        switch operation {
        case .ask(let chats, let target, let show):
            target().map { .ask($0, chats, show: show) }
        case .prompt(let id, let canRun):
            .prompt(id: id, canRun: canRun)
        }
    }

    var isBusy: Bool { !gesture.isIdle }

    /// One `Start` spans the whole recording and answers with the text
    /// once it stops. The daemon types nothing then, since the text is the question.
    ///
    /// Silence returns to idle. A chord has no way to explain itself,
    /// and the daemon's own failure sound already covers a broken recording.
    private func run(_ run: Run) async {
        switch run {
        case .ask(let target, let chats, let show):
            if await ask(target, in: chats) { show() }
            // The chat follows the ask from here, so the next press can start another.
            gesture.abandon()
        case .prompt(let id, let canRun):
            await runPrompt(id: id, canRun: canRun)
        }
    }

    private func runPrompt(id: String, canRun: @MainActor () -> Bool) async {
        let ticket = await prompt(id: id, canRun: canRun)
        guard let ticket else {
            gesture.abandon()
            return
        }
        started(.ticket(ticket))
        gesture.issued(ticket: ticket)
    }

    /// A recording uses the vocabulary of the folder its chat runs in. Returns whether a
    /// question was heard.
    private func ask(_ target: AskTarget, in chats: Chats) async -> Bool {
        switch target {
        case .new:
            guard let chat = await effects.resolve(),
                let heard = await record(projectDirectory: chat.workdir)
            else { return false }
            chats.startNew(heard.text, at: chat, ownVoice: heard.ownVoice)
        case .followUp(let chat):
            let folder = chats.shown.flatMap { $0.key == chat ? $0.folder : nil }
            chats.recording = chat
            defer { chats.recording = nil }
            guard let heard = await record(projectDirectory: folder) else { return false }
            // A rename during recording moves `recording` to the new name.
            chats.followUp(heard.text, in: chats.recording ?? chat, folder: folder, ownVoice: heard.ownVoice)
        }
        return true
    }

    /// A prompt names no project, so the daemon picks the vocabulary.
    private func prompt(id: String, canRun: @MainActor () -> Bool) async -> UInt64? {
        guard canRun(), let heard = await record(projectDirectory: nil) else { return nil }
        return await effects.runPrompt(heard.text, id, pressedIn)
    }

    /// Nil for silence as well as for a recording that failed.
    private func record(projectDirectory: String?) async -> DictationClient.Transcript? {
        let heard = await effects.transcribe(projectDirectory) { [weak self] id in
            self?.started(.recording(id))
        }
        guard let heard, !heard.text.isEmpty else { return nil }
        return heard
    }
}
