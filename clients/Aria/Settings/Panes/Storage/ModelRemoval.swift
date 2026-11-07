import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "ModelRemoval")

/// `aria models remove` refuses a model a setting loads. The Settings list
/// only hides its delete button.
@MainActor
final class ModelRemoval: ObservableObject {
    @Published private(set) var removing: String?
    @Published private(set) var failure: String?

    private let delete: @MainActor (String) async throws -> Void

    init(delete: @escaping @MainActor (String) async throws -> Void = SetupClient.removeModel) {
        self.delete = delete
    }

    /// Refreshes even after a failure, since the files may be partly gone.
    func remove(
        _ model: DownloadedModels.Model, using runner: SetupRunner, then refresh: @MainActor () async -> Void
    ) async {
        guard removing == nil else { return }
        removing = model.id
        failure = nil
        defer { removing = nil }
        do {
            try await runner.run { try await delete(model.id) }
        } catch {
            log.error("Model removal failed: \(String(describing: error), privacy: .private)")
            failure = SetupClientError.message(for: error, otherwise: "Aria couldn’t delete \(model.name). Try again.")
        }
        await refresh()
    }
}
