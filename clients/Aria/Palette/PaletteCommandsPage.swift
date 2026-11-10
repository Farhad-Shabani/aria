import SwiftUI

struct PaletteCommandsPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var systemTranscription: SystemTranscriptionStatus
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            VStack(spacing: 0) {
                searchField
                if controller.model.showsSectionPills {
                    PaletteSectionPills(selected: controller.model.section) { section in
                        controller.model.show(section: section)
                    }
                }
            }
        } content: {
            commandList
                .confirmingDeletion(of: $controller.deletion, from: history)
                .confirmingDeletion(of: $controller.chatDeletion, perform: controller.deleteChat)
        } bottom: {
            PaletteFooter {
                footerNote
            } actions: {
                if controller.model.isRenaming {
                    PaletteTitleActions(controller: controller)
                } else {
                    // Secondary actions, then the one Return runs, pinned to
                    // the right edge so it stays put as the selection moves.
                    let actions = controller.actions
                    PaletteFooterActions(
                        actions: actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
                    if let primaryAction = controller.returnTitle {
                        if !actions.isEmpty {
                            BarDivider()
                        }
                        PaletteFooterButton(title: primaryAction, key: .return) { controller.chooseSelected() }
                    }
                }
            }
        }
        .overlay(alignment: .bottomTrailing) { PaletteActionsMenu(controller: controller, history: history) }
        .confirmationDialog(
            "Clear All History?", isPresented: $controller.model.isConfirmingClear, titleVisibility: .visible
        ) {
            Button("Clear All History", role: .destructive) { history.clear() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "All saved dictations, transcripts, and chats will be deleted, with their recordings "
                    + "and transcript files. This can’t be undone. Your history setting won’t change."
            )
        }
        .announcingNotes(of: history)
        .onChange(of: controller.model.promptFailure) { _, failure in
            if let failure { NSAccessibility.announce(failure.message) }
        }
        .onChange(of: systemTranscription.didFailToStart) { _, didFail in
            guard didFail else { return }
            NSAccessibility.announce(SystemTranscriptionStatus.startFailure)
        }
        .announcingStopFailure(of: systemTranscription)
    }

    private var searchField: some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .frame(width: PaletteRowMetrics.symbolWidth)
                .accessibilityHidden(true)
            if let input = controller.model.input {
                PaletteTag(title: input.name, symbol: input.kind.symbol)
                    .help("Prompts run on this. Esc removes it.")
                    .accessibilityLabel("Prompt input: \(input.name)")
            } else if let section = controller.model.section {
                PaletteTag(title: section.historyTitle, symbol: nil)
                    .help("Esc shows everything again.")
            }
            PaletteSearchField(controller: controller, placeholder: placeholder)
        }
        .padding(.horizontal, PaletteBarMetrics.leading)
        .frame(minHeight: PaletteBarMetrics.headerHeight)
    }

    private var commandList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    let items = controller.model.items
                    let selected = controller.model.selected
                    ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                        // One view per element, so the lazy stack needn't build every row to count them.
                        VStack(alignment: .leading, spacing: 0) {
                            if let day = PaletteModel.dayStarting(at: index, in: items) {
                                Text(HistoryDay.title(for: day))
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                                    .padding(.horizontal, PaletteRowMetrics.padding)
                                    .padding(.top, 10)
                                    .padding(.bottom, 4)
                                    .accessibilityAddTraits(.isHeader)
                            }
                            row(item, isSelected: item == selected)
                                .id(item.id)
                                .onHover { isHovering in
                                    if isHovering { controller.model.select(item.id) }
                                }
                                .onAppear {
                                    if let entry = item.historyItem { history.loadMore(after: entry.id) }
                                }
                        }
                    }
                }
                .padding(PaletteBarMetrics.listInset)
            }
            .overlay {
                if controller.model.items.isEmpty && !history.isLoading {
                    if controller.model.input != nil && controller.model.query.isEmpty {
                        ContentUnavailableView("No Prompts", systemImage: "text.bubble")
                    } else {
                        ContentUnavailableView("No Matches", systemImage: "magnifyingglass")
                    }
                }
            }
            .onChange(of: controller.model.selected?.id) { _, id in
                history.forgetCopied()
                guard let id else { return }
                proxy.scrollTo(id)
                if let title = controller.model.selected?.spokenTitle { NSAccessibility.announce(title) }
            }
        }
    }

    private var placeholder: String {
        if controller.model.input != nil { return "Search prompts" }
        if controller.model.section != nil { return "Search" }
        return controller.model.keepsHistory ? "Search commands and history" : "Search commands"
    }

    @ViewBuilder
    private func row(_ item: PaletteItem, isSelected: Bool) -> some View {
        switch item {
        case .command(let command) where command.action == .showLiveTranscript:
            PaletteLiveRow(
                systemTranscription: systemTranscription, isSelected: isSelected,
                show: { controller.run(command) }, stop: { controller.stopTranscription() })
        case .command(let command):
            PaletteCommandRow(command: command, isSelected: isSelected) { controller.run(command) }
        case .entry(let entry) where entry.id == controller.model.renamingID:
            PaletteEntryRow(
                entry: entry, symbol: entry.kind.symbol, isSelected: isSelected,
                titleField: PaletteTitleField(controller: controller, placeholder: entry.preview)
            ) {}
        case .entry(let entry):
            PaletteEntryRow(entry: entry, symbol: entry.kind.symbol, isSelected: isSelected, badge: badge(for: entry)) {
                controller.choose(item)
            }
        case .chat(let chat):
            // A chat's results stay with each of its asks, on its own page. It names the
            // assistant it asks, since its folder shows on its page.
            let isRenaming = controller.model.renaming == .chat(chat.key)
            PaletteEntryRow(
                entry: chat.newest, symbol: "bubble.left.and.bubble.right", isSelected: isSelected,
                origin: controller.assistantName(of: chat.key), name: chat.name,
                titleField: isRenaming
                    ? PaletteTitleField(controller: controller, placeholder: chat.newest.name) : nil
            ) { controller.choose(item) }
        case .result(let result):
            PaletteResultRow(result: result, isSelected: isSelected) { controller.choose(item) }
        }
    }

    private func badge(for entry: HistoryItem) -> PaletteThreadBadge? {
        guard entry.resultCount > 0 else { return nil }
        return PaletteThreadBadge(
            count: entry.resultCount, isOpen: controller.model.isThreadOpen(of: entry.id)
        ) { controller.toggleThread(of: entry) }
    }

    @ViewBuilder
    private var footerNote: some View {
        if systemTranscription.isStarting {
            PaletteFooterProgress("Starting…")
        } else if systemTranscription.didFailToStart {
            NoticeText(notice: SystemTranscriptionStatus.startFailure)
        } else if systemTranscription.didFailToStop {
            NoticeText(notice: SystemTranscriptionStatus.stopFailure)
        } else if let failure = controller.model.promptFailure {
            NoticeText(notice: failure.message)
        } else if !controller.model.keepsHistory {
            Text("Nothing is saved.")
        } else if let error = history.error {
            NoticeText(notice: error.message)
        } else if history.isClearing {
            PaletteFooterProgress("Clearing…")
        } else if let notice = history.notice {
            Text(notice)
        } else if controller.model.setupSteps > 0 {
            // Outranks Searching…, which only flickers by.
            FinishSetupButton(steps: controller.model.setupSteps) { controller.run(.finishSetup) }
        } else if history.isLoading {
            PaletteFooterProgress("Searching…")
        }
    }

}

private struct FinishSetupButton: View {
    let steps: Int
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(SettingsPane.finishSetup.title)
                CountBadge(count: steps).controlSize(.small)
            }
            .frame(minHeight: PaletteBarMetrics.footerHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// A capsule in the search field naming the prompt input or the history section.
private struct PaletteTag: View {
    private static let maxWidth: CGFloat = 160

    let title: String
    let symbol: String?

    var body: some View {
        HStack(spacing: 4) {
            if let symbol {
                Image(systemName: symbol).accessibilityHidden(true)
            }
            Text(title).lineLimit(1)
        }
        .paletteCapsule()
        // Its own width, up to the cap, so the field starts right after it.
        .frame(maxWidth: Self.maxWidth, alignment: .leading)
        .fixedSize()
    }
}
