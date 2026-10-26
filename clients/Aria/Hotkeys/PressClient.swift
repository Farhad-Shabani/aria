import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "PressClient")

/// The calls a chord makes.
///
/// A failure is logged, never surfaced. There is nowhere to show an error
/// from a global key press, and every outcome that matters to the user is
/// already on the event stream.
enum PressClient {
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
        request.assistantID = target.assistantID
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
