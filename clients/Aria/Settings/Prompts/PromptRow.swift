import SwiftUI

struct PromptRow: View {
    let entry: Aria_Daemon_V1_Prompt
    var flagsMissingFile = false

    var body: some View {
        HStack {
            SettingsIcon(pane: .prompts)
            VStack(alignment: .leading) {
                Text(entry.name).lineLimit(1)
                if flagsMissingFile && !PromptFile.exists(entry) {
                    Text("File not found")
                        .font(.subheadline)
                        .foregroundStyle(.red)
                }
            }
            Spacer()
            PromptShortcutLabel(entry: entry)
        }
    }
}
