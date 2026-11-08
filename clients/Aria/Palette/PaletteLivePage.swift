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
                } else {
                    Text(history.notice ?? SystemTranscriptionStatus.savingNote)
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
