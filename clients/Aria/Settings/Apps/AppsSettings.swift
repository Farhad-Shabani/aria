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
                AppRow(bundleID: entry.applicationID, excluded: entry.excluded)
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

/// Looks its app up once per bundle ID: the lookup reads the disk, and the
/// list redraws on every preference change.
private struct AppRow: View {
    let bundleID: String
    let excluded: Bool

    @State private var app: InstalledApp?

    var body: some View {
        HStack {
            Image(nsImage: app?.icon ?? NSImage())
                .resizable()
                .frame(width: SettingsMetrics.iconSide, height: SettingsMetrics.iconSide)
            Text(app?.name ?? "")
            if excluded {
                Spacer()
                Text("Off").foregroundStyle(.secondary)
            }
        }
        .task(id: bundleID) { app = InstalledApp(bundleID: bundleID) }
    }
}
