import AppKit
import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "PressController")

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
                if await PressRpc.stopRecording(id: id) == nil {
                    recording?.cancel()
                    gesture.abandon()
                }
            }
        case .cancel(let ticket):
            Task { await PressRpc.cancel(ticket: ticket) }
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
        guard let target = await PressRpc.resolve(), !Task.isCancelled,
            let heard = await record(projectDirectory: target.workdir)
        else { return nil }
        return await PressRpc.ask(heard: heard, target: target)
    }

    /// A prompt names no project, so the daemon picks the vocabulary.
    private func runPrompt(id: String) async -> UInt64? {
        guard let heard = await record(projectDirectory: nil) else { return nil }
        return await PressRpc.runPrompt(text: heard.text, promptID: id, application: pressedIn)
    }

    /// Nil for silence as well as for a recording that failed.
    private func record(projectDirectory: String?) async -> Aria_Daemon_V1_DictationResult? {
        let heard = await PressRpc.transcribe(projectDirectory: projectDirectory) { [weak self] id in
            guard let self else { return }
            started(.recording(id))
            act(on: gesture.recordingStarted(id: id))
        }
        guard let heard, !heard.text.isEmpty else { return nil }
        return heard
    }
}

/// The calls a chord makes.
///
/// A failure is logged, never surfaced. There is nowhere to show an error
/// from a global key press, and every outcome that matters to the user is
/// already on the event stream.
private enum PressRpc {
    /// Reads one press to its outcome. The daemon names the recording as
    /// it opens, and `onRecording` gets that name on the main actor, since
    /// a stop can only be sent once it is known.
    ///
    /// Nil for a stop-accepted — another client was recording, so this call
    /// stopped theirs and the transcript went to them — and for a stream
    /// that ends without an outcome.
    static func transcribe(
        projectDirectory: String?,
        onRecording: @escaping @MainActor (UInt64) -> Void
    ) async -> Aria_Daemon_V1_DictationResult? {
        var request = Aria_Daemon_V1_StartDictationRequest()
        request.stopMode = .manual
        request.transcriptOnly = true
        if let projectDirectory {
            request.projectDirectory = projectDirectory
        }
        // A transcript handed back has nowhere to stream to, so the daemon
        // refuses the pairing. Left absent this inherits the stored mode,
        // which is streaming by default.
        request.deliveryMode = .complete
        // Bounded like the Rust client: the daemon ends a press sooner
        // itself, so this only catches one that stopped answering.
        var options = CallOptions.defaults
        options.timeout = Self.pressTimeout
        let sent = request
        let heard = await attempt("record") { [options] client -> Aria_Daemon_V1_DictationResult? in
            try await Aria_Daemon_V1_DictationService.Client(wrapping: client)
                .start(request: ClientRequest(message: sent), options: options) { response in
                    for try await message in response.messages {
                        switch message.body {
                        case .progress(let stage) where stage.stage == .recording:
                            await onRecording(stage.recordingID)
                        case .progress, nil:
                            continue
                        case .result(let result):
                            return result
                        case .stopAccepted:
                            return nil
                        }
                    }
                    return nil
                }
        }
        return heard ?? nil
    }

    /// False when that recording is not the one running; nil when the
    /// stop never reached the daemon.
    static func stopRecording(id: UInt64) async -> Bool? {
        var request = Aria_Daemon_V1_StopDictationRequest()
        request.recordingID = id
        let sent = request
        let response = await attempt("stop recording") { client in
            try await Aria_Daemon_V1_DictationService.Client(wrapping: client)
                .stop(request: ClientRequest(message: sent)) { try $0.message }
        }
        return response?.wasRecording
    }

    static func resolve() async -> Aria_Daemon_V1_ResolveAskResponse? {
        await attempt("resolve the project") { client in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .resolve(request: ClientRequest(message: .init())) { try $0.message }
        }
    }

    static func ask(
        heard: Aria_Daemon_V1_DictationResult, target: Aria_Daemon_V1_ResolveAskResponse
    ) async -> UInt64? {
        var request = Aria_Daemon_V1_AskRequest()
        request.text = heard.text
        request.workdir = target.workdir
        request.agent = target.agent
        // The reply is typed where the user is looking, or copied when the
        // app in front refuses insertion.
        request.outputMode = .insertOrCopy
        // The daemon's own verdict, handed back: it may remember what its
        // owner said.
        request.ownVoice = heard.ownVoice
        let sent = request
        let response = await attempt("ask") { client in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .ask(request: ClientRequest(message: sent)) { try $0.message }
        }
        return response?.ticket
    }

    static func runPrompt(text: String, promptID: String, application: String?) async -> UInt64? {
        var request = Aria_Daemon_V1_RunPromptRequest()
        request.text = text
        request.promptID = promptID
        request.applicationID = application ?? ""
        let sent = request
        let response = await attempt("run the prompt") { client in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .runPrompt(request: ClientRequest(message: sent)) { try $0.message }
        }
        return response?.ticket
    }

    static func cancel(ticket: UInt64) async {
        var request = Aria_Daemon_V1_CancelAskRequest()
        request.ticket = ticket
        let sent = request
        _ = await attempt("cancel") { client in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .cancel(request: ClientRequest(message: sent)) { try $0.message }
        }
    }

    private static let pressTimeout = Duration.seconds(330)

    private static func attempt<R: Sendable>(
        _ what: String,
        _ body: @escaping @Sendable (GRPCClient<HTTP2ClientTransport.Posix>) async throws -> R
    ) async -> R? {
        do {
            return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
                try await body(client)
            }
        } catch {
            log.error("Could not \(what, privacy: .public): \(String(describing: error), privacy: .private)")
            return nil
        }
    }
}
