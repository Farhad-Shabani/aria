import AppKit
import UniformTypeIdentifiers

/// The Markdown file a prompt reads, as it stands on disk.
enum PromptFile {
    /// The prompt run reads no more (`MAX_INSTRUCTIONS_BYTES`). The daemon reads and checks the
    /// file. The app checks only a draft as it is typed.
    static let maximumKilobytes = 16
    static let maximumBytes = maximumKilobytes * 1024

    static func exists(_ entry: Aria_Daemon_V1_Prompt) -> Bool {
        FileManager.default.fileExists(atPath: entry.file)
    }

    /// Nil when the user cancels.
    @MainActor
    static func choose() -> URL? {
        let panel = openPanel()
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url
    }

    @MainActor
    static func openPanel() -> NSOpenPanel {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [UTType(filenameExtension: "md")].compactMap { $0 }
        panel.prompt = "Use File"
        return panel
    }
}
