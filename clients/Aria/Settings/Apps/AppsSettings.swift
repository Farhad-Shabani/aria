import SwiftUI

struct AppsSettings: View {
    let preferences: DaemonPreferences
    /// The app whose page Back just left.
    let marked: String?
    let open: (String) -> Void

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            let apps = editor.snapshot.applicationOverrides
            ItemList(
                items: apps.entries,
                id: \.applicationID,
                noun: "App",
                empty: "No Apps",
                footer: "Other apps use Dictation settings.",
                marked: marked,
                canAdd: apps.entries.count < ApplicationOverrideEdits.maximumApps,
                add: { add(editor) },
                remove: { bundleID in
                    editor.editLatest(\.applicationOverrides, \.applicationOverrides) {
                        ApplicationOverrideEdits.remove(bundleID, from: &$0)
                    }
                },
                open: open
            ) { entry in
                row(entry)
            }
        }
    }

    private func row(_ entry: Aria_Daemon_V1_ApplicationOverride) -> some View {
        let app = InstalledApp(bundleID: entry.applicationID)
        return HStack {
            Image(nsImage: app.icon)
                .resizable()
                .frame(width: SettingsMetrics.iconSide, height: SettingsMetrics.iconSide)
            Text(app.name)
            if entry.excluded {
                Spacer()
                Text("Off").foregroundStyle(.secondary)
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
