/// The calls a press makes.
///
/// A failure is logged by the transport, never surfaced. There is nowhere to
/// show an error from a global key press, and every outcome that matters to
/// the user is already on the event stream.
enum PressClient {
    /// Bounded like the Rust client: the daemon ends a press sooner itself,
    /// so this only catches one that stopped answering.
    private static let pressTimeout = Duration.seconds(670)

    /// Reads one press to its transcript. Nil for a stop-accepted and for a
    /// recording that failed.
    static func transcribe(
        projectDirectory: String?,
        onRecording: @escaping @MainActor (UInt64) -> Void
    ) async -> DictationClient.Transcript? {
        let outcome = try? await DictationClient.start(
            stop: .manual, projectDirectory: projectDirectory, timeout: pressTimeout, onRecording: onRecording
        )
        switch outcome {
        case .heard(let transcript): return transcript
        case .stopAccepted, nil: return nil
        }
    }

    static func resolve() async -> AskTarget? {
        try? await ChatClient.resolve()
    }

    static func ask(heard: DictationClient.Transcript, target: AskTarget) async -> UInt64? {
        try? await ChatClient.ask(text: heard.text, target: target, ownVoice: heard.ownVoice)
    }

    static func runPrompt(text: String, promptID: String, application: String?) async -> UInt64? {
        try? await ChatClient.runPrompt(text: text, promptID: promptID, application: application)
    }
}
