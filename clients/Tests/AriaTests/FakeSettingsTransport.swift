@testable import Aria

final class FakeSettingsTransport: SettingsTransport {
    struct Unreachable: Error {}

    var snapshots: [Aria_Daemon_V1_SettingsSnapshot] = []
    var updates: [Aria_Daemon_V1_UpdateSettingsResponse] = []
    var snapshotError: (any Error)?
    var updateError: (any Error)?
    var duringUpdate: (() async -> Void)?
    var duringSnapshot: (() async -> Void)?
    private(set) var sentUpdates: [Aria_Daemon_V1_UpdateSettingsRequest] = []

    func snapshot() async throws -> Aria_Daemon_V1_SettingsSnapshot {
        let action = duringSnapshot
        duringSnapshot = nil
        await action?()
        if let snapshotError { throw snapshotError }
        guard !snapshots.isEmpty else { throw Unreachable() }
        return snapshots.removeFirst()
    }

    func update(
        _ request: Aria_Daemon_V1_UpdateSettingsRequest
    ) async throws -> Aria_Daemon_V1_UpdateSettingsResponse {
        sentUpdates.append(request)
        let action = duringUpdate
        duringUpdate = nil
        await action?()
        if let updateError { throw updateError }
        guard !updates.isEmpty else { throw Unreachable() }
        return updates.removeFirst()
    }
}
