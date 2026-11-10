import SwiftUI

extension View {
    /// Asks before deleting `entry` from history, since it can’t be undone.
    func confirmingDeletion(of entry: Binding<HistoryItem?>, from history: HistoryState) -> some View {
        confirmationDialog(
            PaletteDeletion.title(entry.wrappedValue?.kind),
            isPresented: Binding(get: { entry.wrappedValue != nil }, set: { if !$0 { entry.wrappedValue = nil } }),
            titleVisibility: .visible,
            presenting: entry.wrappedValue
        ) { entry in
            // Return confirms, so ⌘⌫ then Return deletes, as in Finder.
            Button("Delete", role: .destructive) { history.delete(entry.id) }
                .keyboardShortcut(.defaultAction)
            Button("Cancel", role: .cancel) {}
        } message: { entry in
            Text("“\(String(entry.name.prefix(80)))” will be deleted" + PaletteDeletion.suffix(entry))
        }
    }
}

extension View {
    /// Asks before deleting a chat's saved asks, since it can’t be undone.
    func confirmingDeletion(
        of chat: Binding<ChatKey?>, perform delete: @escaping (ChatKey) -> Void
    ) -> some View {
        confirmationDialog(
            "Delete This Chat?",
            isPresented: Binding(
                get: { chat.wrappedValue != nil }, set: { if !$0 { chat.wrappedValue = nil } }),
            titleVisibility: .visible,
            presenting: chat.wrappedValue
        ) { chat in
            Button("Delete", role: .destructive) { delete(chat) }
                .keyboardShortcut(.defaultAction)
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Its questions, replies, and prompt results will be deleted. The assistant keeps its own records.")
        }
    }
}

private enum PaletteDeletion {
    static func title(_ kind: HistoryItem.Kind?) -> String {
        switch kind {
        case .systemTranscription: "Delete This Transcript?"
        case .ask: "Delete This Chat?"
        case .dictation, nil: "Delete This Dictation?"
        }
    }

    static func suffix(_ entry: HistoryItem) -> String {
        if case .systemTranscription = entry.kind { return " along with its transcript file." }
        if case .available = entry.audio { return " along with its saved recording." }
        return "."
    }
}
