import SwiftUI

/// Where Mac Audio saves transcripts: Aria’s own folder, which never moves.
struct TranscriptsFolderRow: View {
    /// Absolute, as the daemon sends it.
    let folder: String
    @State private var openFailed = false

    var body: some View {
        LabeledContent {
            Button("Open") { openFailed = !Self.open(folder) }
        } label: {
            Text(SettingsRow.transcriptsFolder.title)
            Text((folder as NSString).abbreviatingWithTildeInPath)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .settingsRow(.transcriptsFolder)
        if openFailed {
            NoticeText(notice: "Aria couldn’t open the folder. Try again.")
        }
    }

    /// Makes the folder first, private to its owner, as Aria does for its first transcript.
    private static func open(_ folder: String) -> Bool {
        let url = URL(filePath: folder, directoryHint: .isDirectory)
        do {
            try FileManager.default.createDirectory(
                at: url, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        } catch {
            return false
        }
        return NSWorkspace.shared.open(url)
    }
}
