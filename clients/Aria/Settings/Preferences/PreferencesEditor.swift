import SwiftUI

@MainActor
struct PreferencesEditor {
    let snapshot: Aria_Daemon_V1_UserPreferences
    let preferences: DaemonPreferences

    /// Snapshot and patch name their fields alike, so most controls pass
    /// the same key path twice.
    func binding<Value>(
        _ field: KeyPath<Aria_Daemon_V1_UserPreferences, Value>,
        _ patch: WritableKeyPath<Aria_Daemon_V1_PreferencePatch, Value>
    ) -> Binding<Value> {
        binding(snapshot[keyPath: field]) { value, edit in edit[keyPath: patch] = value }
    }

    /// For an edit that converts the value or touches more than one field.
    func binding<Value>(
        _ value: Value,
        edit: @escaping (Value, inout Aria_Daemon_V1_PreferencePatch) -> Void
    ) -> Binding<Value> {
        Binding(get: { value }, set: { self.set($0, edit: edit) })
    }

    func set<Value>(_ patch: WritableKeyPath<Aria_Daemon_V1_PreferencePatch, Value>, to value: Value) {
        set(value) { value, edit in edit[keyPath: patch] = value }
    }

    /// Builds the patch here, so the edit closure never crosses into the task.
    private func set<Value>(_ value: Value, edit: (Value, inout Aria_Daemon_V1_PreferencePatch) -> Void) {
        var patch = Aria_Daemon_V1_PreferencePatch()
        edit(value, &patch)
        let preferences = preferences
        Task { await preferences.set { patch.apply(to: &$0) } }
    }
}
