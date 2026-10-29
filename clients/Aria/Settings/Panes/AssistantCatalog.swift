import Foundation
import os

@MainActor
final class AssistantCatalog: ObservableObject {
    private enum Failure: Error { case invalidResponse }
    private static let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "AssistantCatalog")

    @Published private(set) var choices: [Aria_Daemon_V1_Assistant] = []
    @Published private(set) var isLoading = false
    @Published private(set) var failure: String?

    private let list: @MainActor () async throws -> [Aria_Daemon_V1_Assistant]
    private var loadID: UUID?

    init(list: @escaping @MainActor () async throws -> [Aria_Daemon_V1_Assistant] = AssistantClient.assistants) {
        self.list = list
    }

    func loadIfEmpty() async {
        guard choices.isEmpty else { return }
        await load()
    }

    func load() async {
        let loadID = UUID()
        self.loadID = loadID
        isLoading = true
        failure = nil
        defer {
            if self.loadID == loadID { isLoading = false }
        }
        do {
            let choices = try await list()
            guard !Task.isCancelled, self.loadID == loadID else { return }
            guard !choices.isEmpty, Set(choices.map(\.id)).count == choices.count,
                choices.allSatisfy({ choice in
                    switch choice.id {
                    case .unspecified, .UNRECOGNIZED: return false
                    default:
                        return !choice.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                            && choice.name.utf8.count <= 200
                            && !choice.name.unicodeScalars.contains { CharacterSet.controlCharacters.contains($0) }
                    }
                })
            else { throw Failure.invalidResponse }
            self.choices = choices
        } catch {
            guard !Task.isCancelled, self.loadID == loadID else { return }
            Self.log.error("Assistant catalog failed: \(String(describing: error), privacy: .private)")
            failure = "Assistants couldn't be loaded. Try again."
        }
    }
}
