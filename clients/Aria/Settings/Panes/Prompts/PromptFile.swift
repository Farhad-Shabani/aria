import AppKit
import UniformTypeIdentifiers

/// The Markdown file a prompt reads, as it stands on disk.
enum PromptFile {
    static func exists(_ entry: Aria_Daemon_V1_Prompt) -> Bool {
        FileManager.default.fileExists(atPath: entry.instructionsPath)
    }

    static func isReadable(_ entry: Aria_Daemon_V1_Prompt) -> Bool {
        FileManager.default.isReadableFile(atPath: entry.instructionsPath)
    }

    /// Nil when the user cancels.
    @MainActor
    static func choose() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [UTType(filenameExtension: "md")].compactMap { $0 }
        panel.prompt = "Use File"
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url
    }
}
