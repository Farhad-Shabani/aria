import SwiftUI

@MainActor
struct SettingsEditor {
    let snapshot: Aria_Daemon_V1_Settings
    let settings: DaemonSettings

    /// Snapshot and patch name their fields alike, so most controls pass
    /// the same key path twice.
    func binding<Value>(
        _ field: KeyPath<Aria_Daemon_V1_Settings, Value>,
        _ patch: WritableKeyPath<Aria_Daemon_V1_SettingsPatch, Value>
    ) -> Binding<Value> {
        binding(snapshot[keyPath: field]) { value, edit in edit[keyPath: patch] = value }
    }

    /// For an edit that converts the value or touches more than one field.
    func binding<Value>(
        _ value: Value,
        edit: @escaping (Value, inout Aria_Daemon_V1_SettingsPatch) -> Void
    ) -> Binding<Value> {
        Binding(get: { value }, set: { self.set($0, edit: edit) })
    }

    /// For a field the daemon replaces whole, such as a list. Each change
    /// starts from the latest snapshot rather than the one this view drew,
    /// so two quick edits don't undo each other.
    func editLatest<Value>(
        _ field: KeyPath<Aria_Daemon_V1_Settings, Value> & Sendable,
        _ patch: WritableKeyPath<Aria_Daemon_V1_SettingsPatch, Value> & Sendable,
        _ change: @escaping @MainActor (inout Value) -> Void
    ) {
        let settings = settings
        Task {
            guard var value = settings.snapshot?[keyPath: field] else { return }
            change(&value)
            await settings.set { $0[keyPath: patch] = value }
        }
    }

    func set<Value>(_ patch: WritableKeyPath<Aria_Daemon_V1_SettingsPatch, Value>, to value: Value) {
        set(value) { value, edit in edit[keyPath: patch] = value }
    }

    /// Builds the patch here, so the edit closure never crosses into the task.
    func set<Value>(_ value: Value, edit: (Value, inout Aria_Daemon_V1_SettingsPatch) -> Void) {
        var patch = Aria_Daemon_V1_SettingsPatch()
        edit(value, &patch)
        let settings = settings
        Task { await settings.set { patch.apply(to: &$0) } }
    }
}
