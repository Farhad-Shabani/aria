import SwiftUI

/// One app's page inside Apps.
struct ApplicationSettings: View {
    let bundleID: String
    let preferences: DaemonPreferences

    /// Looked up once per app, not on every preference change.
    @State private var app: InstalledApp?

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            let apps = editor.snapshot.applicationOverrides
            if let overrides = ApplicationOverrideEdits.overrides(for: bundleID, in: apps) {
                let excluded = ApplicationOverrideEdits.isExcluded(bundleID, in: apps)
                ItemPage(title: app?.name ?? "") {
                    Image(nsImage: app?.icon ?? NSImage())
                        .resizable()
                        .frame(width: SettingsMetrics.headerIconSide, height: SettingsMetrics.headerIconSide)
                } content: {
                    Section {
                        // The accent color reads as switched on, the opposite of this switch.
                        Toggle("Turn Off Aria", isOn: turnedOff(excluded, editor))
                            .tint(.red)
                        if !excluded {
                            ForEach(AppSetting.shown(for: bundleID, overrides: overrides), id: \.self) { setting in
                                control(setting, overrides: overrides, editor)
                            }
                        }
                    }
                }
            }
        }
        .task(id: bundleID) { app = InstalledApp(bundleID: bundleID) }
    }

    private func turnedOff(_ excluded: Bool, _ editor: PreferencesEditor) -> Binding<Bool> {
        let bundleID = bundleID
        return Binding(
            get: { excluded },
            set: { off in
                editor.editLatest(\.applicationOverrides, \.applicationOverrides) {
                    ApplicationOverrideEdits.setExcluded(off, for: bundleID, in: &$0)
                }
            }
        )
    }

    @ViewBuilder
    private func control(
        _ setting: AppSetting,
        overrides: Aria_Daemon_V1_DictationOverrides,
        _ editor: PreferencesEditor
    ) -> some View {
        let global = editor.snapshot
        switch setting {
        case .output:
            picker(
                setting.title,
                modes: DictationEdits.outputModes,
                chosen: overrides.hasOutputMode ? overrides.outputMode : nil,
                dictation: global.outputMode,
                title: \.title,
                editor,
                ApplicationOverrideEdits.setOutput
            )
        case .delivery:
            let output = overrides.hasOutputMode ? overrides.outputMode : global.outputMode
            let locked = DictationEdits.locksDelivery(output)
            picker(
                setting.title,
                modes: DictationEdits.deliveryModes,
                chosen: locked ? .complete : (overrides.hasDeliveryMode ? overrides.deliveryMode : nil),
                dictation: global.deliveryMode,
                title: \.title,
                editor,
                ApplicationOverrideEdits.setDelivery
            )
            .disabled(locked)
        }
    }

    /// Lists each mode once. Dictation's own choice is marked Default and
    /// picking it clears the app's setting.
    private func picker<Mode: Hashable>(
        _ label: String,
        modes: [Mode],
        chosen: Mode?,
        dictation: Mode,
        title: KeyPath<Mode, String>,
        _ editor: PreferencesEditor,
        _ edit: @escaping (Mode?, String, inout Aria_Daemon_V1_ApplicationOverrides) -> Void
    ) -> some View {
        let bundleID = bundleID
        let selection = Binding(
            get: { ApplicationOverrideEdits.choice(chosen, dictation: dictation) },
            set: { mode in
                editor.editLatest(\.applicationOverrides, \.applicationOverrides) { edit(mode, bundleID, &$0) }
            }
        )
        return Picker(label, selection: selection) {
            ForEach(modes, id: \.self) { mode in
                let isDefault = mode == dictation
                Text(isDefault ? "\(mode[keyPath: title]) (Default)" : mode[keyPath: title])
                    .tag(isDefault ? nil : Optional(mode))
            }
        }
    }
}
