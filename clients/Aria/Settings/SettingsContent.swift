import SwiftUI

struct SettingsContent<Content: View>: View {
    @ObservedObject var settings: DaemonSettings
    @ViewBuilder let content: (SettingsEditor) -> Content

    var body: some View {
        if let snapshot = settings.snapshot {
            content(SettingsEditor(snapshot: snapshot, settings: settings))
        } else if let failure = settings.loadFailure {
            ContentUnavailableView {
                Label("Settings Unavailable", systemImage: "exclamationmark.triangle")
            } description: {
                Text(markdown: failure)
            } actions: {
                Button("Try Again") { Task { await settings.sync() } }
            }
        } else {
            // No error yet means the first load is still out.
            DaemonWaitingView(isLoading: settings.lastError == nil)
        }
    }
}
