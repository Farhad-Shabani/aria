import SwiftUI

/// The Mac Audio session's transcript, as it streams in.
struct PaletteLivePage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var listen: ListenStatus
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            header
        } content: {
            ListenTranscriptView(transcript: listen.transcript, isRunning: listen.isRunning)
        } bottom: {
            PaletteFooter {
                if listen.didFailToStop {
                    NoticeText(notice: ListenStatus.stopFailure)
                } else if let error = history.error {
                    NoticeText(notice: error.message)
                } else {
                    Text(history.notice ?? ListenStatus.savingNote)
                }
            } actions: {
                PaletteFooterActions(
                    actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
            }
        }
        .announcingNotes(of: history)
        .announcingStopFailure(of: listen)
    }

    private var header: some View {
        PaletteHeader(back: controller.escape) {
            Text("Mac Audio")
        } detail: {
            ListenClock(listen)
        }
    }
}
