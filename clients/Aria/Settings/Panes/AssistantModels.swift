import Foundation
import os

@MainActor
final class AssistantModels: ObservableObject {
    private static let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "AssistantModels")

    @Published private(set) var choices: [Aria_Daemon_V1_AssistantModel] = []
    @Published private(set) var isLoading = false
    @Published private(set) var failure: String?

    private let list: @MainActor (Aria_Daemon_V1_AssistantId) async throws -> [Aria_Daemon_V1_AssistantModel]
    private var loadID: UUID?

    init(
        list: @escaping @MainActor (Aria_Daemon_V1_AssistantId) async throws -> [Aria_Daemon_V1_AssistantModel] =
            AssistantClient.models
    ) {
        self.list = list
    }

    func load(_ assistantID: Aria_Daemon_V1_AssistantId) async {
        let loadID = UUID()
        self.loadID = loadID
        choices = []
        failure = nil
        switch assistantID {
        case .unspecified, .UNRECOGNIZED:
            isLoading = false
            return
        default: break
        }
        isLoading = true
        defer {
            if self.loadID == loadID { isLoading = false }
        }
        do {
            let choices = try await list(assistantID)
            guard !Task.isCancelled, self.loadID == loadID else { return }
            self.choices = choices
        } catch {
            guard !Task.isCancelled, self.loadID == loadID else { return }
            Self.log.error("Model list failed: \(String(describing: error), privacy: .private)")
            failure = "Aria couldn’t load the models. Try again, or enter a model ID."
        }
    }
}
