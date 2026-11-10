import SwiftUI

struct PaletteEntryPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            header
        } content: {
            content.disabled(history.isClearing)
        } bottom: {
            PaletteFooter {
                footerNote
            } actions: {
                actions
            }
        }
        .onChange(of: history.selectedID) { _, id in
            if id == nil { controller.model.show(.commands) }
        }
        .overlay(alignment: .bottomTrailing) { PaletteActionsMenu(controller: controller, history: history) }
        .confirmingDeletion(of: $controller.deletion, from: history)
        .announcingNotes(of: history)
    }

    private var header: some View {
        PaletteHeader(back: controller.escape) {
            if let entry = history.detail, entry.id == controller.model.renamingID {
                PaletteTitleField(controller: controller, placeholder: entry.kind.title, style: .headline)
            } else {
                Text(history.detail.map { $0.title ?? $0.kind.title } ?? "History")
            }
        } detail: {
            if let entry = history.detail {
                Text(entry.created.formatted(date: .abbreviated, time: .shortened))
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        if let entry = history.detail {
            PaletteEntryDetail(
                entry: entry, thread: controller.model.threadResults(of: entry.id),
                focus: controller.model.focusedResult(on: entry.id)?.id
            ) { history.copy(text: $0.text) }
        } else if history.isLoadingDetail {
            PalettePageProgress("Loading…")
        }
    }

    @ViewBuilder
    private var footerNote: some View {
        if let error = history.error {
            NoticeText(notice: error.message)
        } else if let notice = history.notice {
            Text(notice)
        } else if let entry = history.detail, entry.isRegenerating || history.regeneratingID == entry.id {
            PaletteFooterProgress("Transcribing…")
        } else if let entry = history.detail {
            Text(entry.facts)
        }
    }

    @ViewBuilder
    private var actions: some View {
        if controller.model.isRenaming {
            PaletteTitleActions(controller: controller)
        } else {
            PaletteFooterActions(
                actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
        }
    }
}
