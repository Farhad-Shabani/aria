import SwiftUI

struct PreferencesForm<Content: View>: View {
    let preferences: DaemonPreferences
    @ViewBuilder let content: (PreferencesEditor) -> Content

    var body: some View {
        PreferencesContent(preferences: preferences) { editor in
            Form { content(editor) }
        }
    }
}
