import SwiftUI

struct PromptRow: View {
    @State private var isMissingFile = false
    @State private var fileCheckVersion = 0
    let entry: Aria_Daemon_V1_Prompt
    var flagsMissingFile = false
    var isSelected = false

    var body: some View {
        HStack {
            SettingsIcon(pane: .prompts)
            VStack(alignment: .leading) {
                Text(entry.name).lineLimit(1)
                if flagsMissingFile && isMissingFile {
                    Text("File not found")
                        .font(.subheadline)
                        .foregroundStyle(isSelected ? Color.primary : .failure)
                }
            }
            Spacer()
            PromptShortcutLabel(entry: entry)
        }
        .task(id: fileCheckVersion) { await refreshFileStatus() }
        .onChange(of: entry.instructionsPath) { _, _ in
            isMissingFile = false
            fileCheckVersion += 1
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { _ in
            if flagsMissingFile { fileCheckVersion += 1 }
        }
    }

    private func refreshFileStatus() async {
        guard flagsMissingFile else { return }
        let entry = entry
        let exists = await Task.detached(priority: .utility) { PromptFile.exists(entry) }.value
        guard !Task.isCancelled else { return }
        isMissingFile = !exists
    }
}
