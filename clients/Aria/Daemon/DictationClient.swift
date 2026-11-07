import GRPCCore
import GRPCProtobuf

/// One recording handed back as text, over the daemon's DictationService.
enum DictationClient {
    enum Stop {
        case manual
        case onSilence
    }

    struct Transcript: Equatable, Sendable {
        let text: String
        /// The daemon's verdict on whose voice it heard.
        let ownVoice: Bool
    }

    enum Outcome: Equatable, Sendable {
        case heard(Transcript)
        /// Another client was recording, so this call stopped theirs and the
        /// transcript went to them.
        case stopAccepted
    }

    /// The daemon ends a press sooner itself, so this only catches a daemon that stopped answering.
    static let pressTimeout = Duration.seconds(670)

    /// A dictation kept in history and typed nowhere. The Aria key stops it. The transport logs a
    /// failure, and the pill shows how it ended.
    static func dictateToHistory() async {
        _ = try? await start(stop: .manual, projectDirectory: nil, timeout: pressTimeout, savesToHistory: true)
    }

    /// Reads one recording to its outcome. The daemon names the recording as
    /// it opens, and `onRecording` gets that name on the main actor, so the
    /// pill can mark it. A stream that ends without an outcome is an
    /// `invalidResponse`.
    static func start(
        stop: Stop,
        projectDirectory: String?,
        timeout: Duration,
        savesToHistory: Bool = false,
        onRecording: @escaping @MainActor (UInt64) -> Void = { _ in }
    ) async throws -> Outcome {
        var request = Aria_Daemon_V1_StartDictationRequest()
        request.stopMode =
            switch stop {
            case .manual: .manual
            case .onSilence: .onSilence
            }
        request.route = savesToHistory ? .historyOnly(.init()) : .transcriptOnly(.init())
        if let projectDirectory {
            request.projectDirectory = projectDirectory
        }
        let sent = request
        return try await DaemonTransport.call(timeout: timeout) { client, options in
            try await Aria_Daemon_V1_DictationService.Client(wrapping: client)
                .start(request: ClientRequest(message: sent), options: options) { response in
                    for try await message in response.messages {
                        switch message.body {
                        case .progress(let stage) where stage.stage == .recording:
                            await onRecording(stage.recordingID)
                        case .progress, nil:
                            continue
                        case .result(let result):
                            return .heard(Transcript(text: result.text, ownVoice: result.ownVoice))
                        case .stopAccepted:
                            return .stopAccepted
                        }
                    }
                    throw DaemonFailure.invalidResponse
                }
        }
    }
}
