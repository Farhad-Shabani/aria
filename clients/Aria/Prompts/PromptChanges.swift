import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "PromptChanges")

@MainActor
struct PromptChanges {
    var send: (Aria_Daemon_V1_SavePromptRequest) async throws -> Aria_Daemon_V1_SavePromptResponse =
        ChatClient.savePrompt
    var drop: (String) async throws -> Void = { try await ChatClient.removePrompt(id: $0) }
    /// Runs once a change lands, so a page can show it before the settings
    /// watch brings it.
    var refresh: () async -> Void = {}

    func save(
        _ request: Aria_Daemon_V1_SavePromptRequest
    ) async -> Result<Aria_Daemon_V1_SavePromptResponse, PromptDraft.Failure> {
        do {
            let saved = try await send(request)
            await refresh()
            return .success(saved)
        } catch {
            log.error("Saving a prompt failed: \(String(describing: error), privacy: .private)")
            return .failure(PromptDraft.Failure(error))
        }
    }

    /// A prompt another client removed first is off the list already.
    func remove(_ id: String) async -> PromptDraft.Failure? {
        do {
            try await drop(id)
        } catch DaemonFailure.refused(.promptMissing) {
        } catch {
            log.error("Removing a prompt failed: \(String(describing: error), privacy: .private)")
            return PromptDraft.Failure(error)
        }
        await refresh()
        return nil
    }
}
