import Foundation

/// The app's one setup run at a time. `aria setup` holds a lock for a whole
/// run, so a second one would fail as busy. Every run in the app starts here,
/// and every control that starts one is disabled while `isRunning`.
@MainActor
final class SetupRunner: ObservableObject {
    @Published private(set) var isRunning = false

    /// Throws `SetupClientError.busy` while another run holds the slot.
    func run<Value>(_ work: () async throws -> Value) async throws -> Value {
        guard !isRunning else { throw SetupClientError.busy }
        isRunning = true
        defer { isRunning = false }
        return try await work()
    }
}
