/// The calls a press makes.
///
/// Once a run starts, the event stream carries its outcome. Only a prompt
/// run's call reports its failure, since a run that never started sends no
/// event.
enum PressClient {
    /// Reads one press to its text. Nil for a stop-accepted and for a
    /// recording that failed.
    static func transcribe(
        projectDirectory: String?,
        onRecording: @escaping @MainActor (UInt64) -> Void
    ) async -> DictationClient.Transcript? {
        let outcome = try? await DictationClient.start(
            stop: .manual, projectDirectory: projectDirectory, timeout: DictationClient.pressTimeout,
            onRecording: onRecording
        )
        switch outcome {
        case .heard(let transcript): return transcript
        case .stopAccepted, nil: return nil
        }
    }

    static func resolve() async -> ChatTarget? {
        try? await ChatClient.resolve()
    }

    static func runPrompt(text: String, promptID: String, application: String?) async throws -> UInt64 {
        try await ChatClient.runPrompt(text: text, promptID: promptID, application: application)
    }
}
