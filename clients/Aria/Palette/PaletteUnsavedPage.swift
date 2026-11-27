import SwiftUI

/// One unsaved transcript, to save where the person chooses, or discard.
struct PaletteUnsavedPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var unsaved: UnsavedTranscripts
    let transcript: UnsavedTranscript
    @State private var text: UnsavedTranscriptText?

    var body: some View {
        PaletteBars {
            PaletteHeader(back: controller.escape) {
                Text(UnsavedTranscripts.title)
            } detail: {
                Text(detail)
            }
        } content: {
            content
        } bottom: {
            PaletteFooter {
                if let error = unsaved.error {
                    NoticeText(notice: error)
                } else if unsaved.isHistoryFull && controller.model.keepsHistory {
                    Text(UnsavedTranscripts.historyFullNotice)
                }
            } actions: {
                PaletteFooterActions(
                    actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
            }
        }
        .task(id: transcript) {
            // History may have made room since the list was read.
            await unsaved.refresh()
            text = await unsaved.text(of: transcript)
        }
        .confirmationDialog(
            "Discard This Transcript?", isPresented: $controller.model.isConfirmingTranscriptDiscard,
            titleVisibility: .visible
        ) {
            Button("Save…") { controller.saveShownTranscript() }
            Button("Discard", role: .destructive) { controller.discardShownTranscript() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Its transcript file will be deleted. This can’t be undone.")
        }
        .onChange(of: unsaved.error) { _, error in
            if let error { NSAccessibility.announce(error) }
        }
    }

    private var detail: String {
        let started = transcript.startedAt.formatted(date: .abbreviated, time: .shortened)
        return "\(started) · \(Duration.seconds(transcript.duration.rounded()).lengthLabel)"
    }

    @ViewBuilder
    private var content: some View {
        switch text {
        case nil:
            PalettePageProgress("Loading…")
        case .unreadable:
            ContentUnavailableView(
                "Can’t Read Transcript", systemImage: "doc.questionmark",
                description: Text("Aria can’t read this transcript file. You can still save or discard it."))
        case .unavailable:
            ContentUnavailableView(
                "Aria Isn’t Running", systemImage: "power",
                description: Text(DaemonFailure.offlineMessage))
        case .whole(let words), .truncated(let words):
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if case .truncated = text {
                        Text(PaletteEntryDetail.cutShortNote(file: transcript.path)).foregroundStyle(.secondary)
                    }
                    Text(words).textSelection(.enabled)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, PageLayout.textInset)
                .padding(.vertical, PageLayout.verticalInset)
            }
        }
    }
}

/// An unsaved transcript's row, pinned at the top of the commands page.
struct PaletteUnsavedRow: View {
    let transcript: UnsavedTranscript
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        PaletteRow(symbol: HistorySection.systemTranscription.symbol, isSelected: isSelected, action: action) {
            Text(UnsavedTranscripts.title)
                .paletteRowText()
        } accessory: {
            // The date sits outside the row's button, as an entry row's place does, so its tap gesture opens the row.
            Text(transcript.startedAt.formatted(date: .abbreviated, time: .shortened))
                .paletteRowDetail()
                .contentShape(Rectangle())
                .onTapGesture(perform: action)
        }
    }
}
