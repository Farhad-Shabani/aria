import SwiftUI

struct AppsSettings: View {
    let preferences: DaemonPreferences
    @Binding var selected: String?
    let open: (String) -> Void

    var body: some View {
        PreferencesContent(preferences: preferences) { editor in
            let apps = editor.snapshot.applicationOverrides
            ItemList(
                items: apps.entries,
                id: \.applicationID,
                noun: "App",
                empty: "No Apps",
                selected: $selected,
                canAdd: apps.entries.count < ApplicationOverrideEdits.maximumApps,
                add: { add(editor) },
                remove: { bundleID in
                    editor.editLatest(\.applicationOverrides, \.applicationOverrides) {
                        ApplicationOverrideEdits.remove(bundleID, from: &$0)
                    }
                },
                open: open
            ) { entry in
                AppRow(bundleID: entry.applicationID, isExcluded: entry.excluded)
            }
        }
    }

    private func add(_ editor: PreferencesEditor) {
        guard let bundleID = InstalledApp.choose() else { return }
        editor.editLatest(\.applicationOverrides, \.applicationOverrides) {
            ApplicationOverrideEdits.add(bundleID, to: &$0)
        }
        open(bundleID)
    }
}

private struct AppRow: View {
    let bundleID: String
    let isExcluded: Bool

    var body: some View {
        let app = InstalledApp.lookUp(bundleID)
        HStack {
            Image(nsImage: app.icon)
                .resizable()
                .frame(width: SettingsMetrics.iconSide, height: SettingsMetrics.iconSide)
            Text(app.name)
            if isExcluded {
                Spacer()
                Text("Off").foregroundStyle(.secondary)
            }
        }
    }
}
