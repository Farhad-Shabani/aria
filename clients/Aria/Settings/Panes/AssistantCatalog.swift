import Foundation
import os

@MainActor
final class AssistantCatalog: ObservableObject {
    private static let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "AssistantCatalog")

    @Published private(set) var choices: [Assistant] = []
    @Published private(set) var isLoading = false
    @Published private(set) var failure: String?

    private let list: @MainActor () async throws -> [Assistant]
    private var loadID: UUID?

    init(list: @escaping @MainActor () async throws -> [Assistant] = ChatClient.assistants) {
        self.list = list
    }

    func loadUnlessReady() async {
        guard choices.isEmpty || choices.contains(where: { $0.status != .ready }) else { return }
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
            self.choices = choices
        } catch {
            guard !Task.isCancelled, self.loadID == loadID else { return }
            Self.log.error("Assistant catalog failed: \(String(describing: error), privacy: .private)")
            failure = "Aria couldn’t load the assistants. Try again."
        }
    }
}
