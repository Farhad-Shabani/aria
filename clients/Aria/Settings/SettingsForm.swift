import SwiftUI

struct SettingsForm<Content: View>: View {
    let settings: DaemonSettings
    @ViewBuilder let content: (SettingsEditor) -> Content

    var body: some View {
        SettingsContent(settings: settings) { editor in
            Form { content(editor) }
        }
    }
}
