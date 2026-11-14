import SwiftUI

struct MicrophonePicker: View {
    let editor: SettingsEditor
    @StateObject private var microphones = Microphones()

    var body: some View {
        let chosen = editor.snapshot.microphone
        let names = microphones.names
        Picker(selection: editor.binding(\.microphone, \.microphone)) {
            ForEach(Microphones.choices(listed: microphones.list, chosen: chosen, names: names)) { choice in
                Text(choice.title).tag(choice.id)
            }
        } label: {
            Text(SettingsRow.microphone.title)
            if let detail = microphones.failure
                ?? Microphones.caption(listed: microphones.list, chosen: chosen, names: names)
            {
                Text(detail)
            }
        }
        .task { await microphones.run() }
    }
}
