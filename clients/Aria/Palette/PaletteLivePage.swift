import SwiftUI

/// The transcript, as it streams in.
struct PaletteLivePage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var systemTranscription: SystemTranscriptionStatus
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            header
        } content: {
            SystemTranscriptView(transcript: systemTranscription.transcript, isRunning: systemTranscription.isRunning)
        } bottom: {
            PaletteFooter {
                if systemTranscription.didFailToStop {
                    NoticeText(notice: SystemTranscriptionStatus.stopFailure)
                } else if let error = history.error {
                    NoticeText(notice: error.message)
                } else if let notice = history.notice {
                    Text(notice)
                } else if let file = systemTranscription.transcriptFile {
                    TranscriptFileNote(file: file, isRunning: systemTranscription.isRunning)
                } else {
                    Text(SystemTranscriptionStatus.savingNote)
                }
            } actions: {
                PaletteFooterActions(
                    actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
            }
        }
        .announcingNotes(of: history)
        .announcingStopFailure(of: systemTranscription)
    }

    private var header: some View {
        PaletteHeader(back: controller.escape) {
            Text("Live Transcript")
        } detail: {
            SystemTranscriptionClock(systemTranscription)
        }
    }
}

private struct TranscriptFileNote: View {
    private static let showInFinder = "Show in Finder"

    let file: URL
    let isRunning: Bool
    @State private var isHovered = false

    var body: some View {
        HStack(spacing: 3) {
            Text(isRunning ? "Saving to" : "Saved to")
            Button {
                NSWorkspace.shared.activateFileViewerSelecting([file])
            } label: {
                Text(file.lastPathComponent).underline(isHovered)
            }
            .buttonStyle(.link)
            .truncationMode(.middle)
            .pointerStyle(.link)
            .onHover { isHovered = $0 }
            .help(Self.showInFinder)
            .accessibilityHint(Self.showInFinder)
        }
    }
}
