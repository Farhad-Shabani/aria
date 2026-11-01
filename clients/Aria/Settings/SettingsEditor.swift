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

    /// For a field the daemon replaces whole, such as a list. `change` runs on
    /// this client's newest copy of the field each time the edit runs, not on
    /// the value from when the person made it. So two quick edits both apply,
    /// and a retry keeps entries another client added.
    func editLatest<Value>(
        _ field: KeyPath<Aria_Daemon_V1_Settings, Value> & Sendable,
        _ patch: WritableKeyPath<Aria_Daemon_V1_SettingsPatch, Value> & Sendable,
        _ change: @escaping @MainActor (inout Value) -> Void
    ) {
        let settings = settings
        Task {
            await settings.edit { base in
                var value = base[keyPath: field]
                change(&value)
                var edit = Aria_Daemon_V1_SettingsPatch()
                edit[keyPath: patch] = value
                return edit
            }
        }
    }

    /// Asks the daemon whether it would accept the edit. Writes nothing.
    func check(_ edit: (inout Aria_Daemon_V1_SettingsPatch) -> Void) async throws {
        var request = Aria_Daemon_V1_CheckSettingsRequest()
        edit(&request.patch)
        try await UnixSocketSettings().check(request)
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
