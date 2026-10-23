import SwiftUI

struct PreferencesForm<Content: View>: View {
    @ObservedObject var preferences: DaemonPreferences
    @ViewBuilder let content: (PreferencesEditor) -> Content

    var body: some View {
        if let snapshot = preferences.snapshot {
            Form {
                content(PreferencesEditor(snapshot: snapshot, preferences: preferences))
            }
        } else if let failure = preferences.loadFailure {
            ContentUnavailableView {
                Label("Settings Unavailable", systemImage: "exclamationmark.triangle")
            } description: {
                Text(failure)
            } actions: {
                Button("Try Again") { Task { await preferences.sync() } }
            }
        } else {
            DaemonWaitingView()
        }
    }
}
