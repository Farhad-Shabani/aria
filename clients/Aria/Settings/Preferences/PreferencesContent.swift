import SwiftUI

struct PreferencesContent<Content: View>: View {
    @ObservedObject var preferences: DaemonPreferences
    @ViewBuilder let content: (PreferencesEditor) -> Content

    var body: some View {
        if let snapshot = preferences.snapshot {
            content(PreferencesEditor(snapshot: snapshot, preferences: preferences))
        } else if let failure = preferences.loadFailure {
            ContentUnavailableView {
                Label("Settings Unavailable", systemImage: "exclamationmark.triangle")
            } description: {
                Text(markdown: failure)
            } actions: {
                Button("Try Again") { Task { await preferences.sync() } }
            }
        } else {
            // No error yet means the first load is still out.
            DaemonWaitingView(isLoading: preferences.lastError == nil)
        }
    }
}
