import SwiftUI

struct PalettePromptPage: View {
    @ObservedObject var controller: PaletteController
    @Binding var draft: PromptDraft
    @FocusState private var isNameFocused: Bool

    var body: some View {
        PaletteBars {
            PaletteHeader(back: controller.escape) {
                Text(draft.isNew ? "New Prompt" : "Edit Prompt")
            }
        } content: {
            PalettePromptForm(draft: $draft, isNameFocused: $isNameFocused, chooseFile: controller.chooseFile)
        } bottom: {
            PaletteFooter {
                if draft.failure != nil {
                    NoticeText(notice: draft.note)
                } else if let note = draft.note {
                    Text(note)
                }
            } actions: {
                PaletteFooterButton(title: "Cancel", key: .escape) { controller.escape() }
                BarDivider()
                PaletteFooterButton(title: "Save", key: .save) { controller.saveDraft() }
                    .disabled(draft.problem != nil)
            }
        }
        // Focus asked for while the page is being built is dropped, since the field isn't in the
        // window yet. A task runs once the field is.
        .task { isNameFocused = true }
        .confirmationDialog(
            "Discard Changes?", isPresented: $controller.model.isConfirmingDiscard, titleVisibility: .visible
        ) {
            Button("Discard", role: .destructive) { controller.model.discardDraft() }
            Button("Keep Editing", role: .cancel) { controller.model.keepDraft() }
        } message: {
            Text(draft.isNew ? "This prompt won’t be saved." : "Your changes to this prompt won’t be saved.")
        }
        .onChange(of: draft.note) { _, note in
            if let note { NSAccessibility.announce(note) }
        }
    }
}
