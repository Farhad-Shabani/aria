import SwiftUI

struct PaletteFooter<Note: View, Actions: View>: View {

    @ViewBuilder let note: Note
    @ViewBuilder let actions: Actions

    var body: some View {
        HStack(spacing: 4) {
            note
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            actions
        }
        // With no note the row is only its actions, and they still sit right.
        .frame(maxWidth: .infinity, alignment: .trailing)
        .font(.caption)
        .foregroundStyle(.secondary)
        .padding(.leading, PaletteBarMetrics.leading)
        .padding(.trailing, PaletteBarMetrics.trailing)
        .frame(minHeight: PaletteBarMetrics.footerHeight)
    }
}

struct PaletteFooterProgress: View {
    private let title: String

    var body: some View {
        HStack(spacing: 6) {
            ProgressView().controlSize(.mini)
            Text(title)
        }
    }
}

extension PaletteFooterProgress {
    init(_ title: String) {
        self.title = title
    }
}

struct PaletteFooterButton: View {
    let title: String
    let key: PaletteKey
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title)
                PaletteKeyCap(key.label).accessibilityHidden(true)
            }
            .padding(.horizontal, 6)
            .frame(minHeight: PaletteBarMetrics.footerHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// Takes the actions from the page, which observes what they are built from.
struct PaletteFooterActions: View {
    let actions: [PaletteAction]
    let perform: (PaletteAction) -> Void
    let toggleMenu: () -> Void

    var body: some View {
        ForEach(actions.filter(\.isInFooter)) { action in
            PaletteFooterButton(title: action.title, key: action.key) { perform(action) }
                .disabled(!action.isEnabled)
        }
        if actions.contains(where: { !$0.isInFooter }) {
            PaletteFooterButton(title: "Actions", key: .actions, action: toggleMenu)
        }
    }
}

extension View {
    /// Speaks history's errors and notices, such as “Copied”, as the footer
    /// shows them, since the palette's focus stays put.
    func announcingNotes(of history: HistoryState) -> some View {
        onChange(of: history.error?.message) { _, message in
            if let message { NSAccessibility.announce(message) }
        }
        .onChange(of: history.notice) { _, notice in
            if let notice { NSAccessibility.announce(notice) }
        }
    }
}
