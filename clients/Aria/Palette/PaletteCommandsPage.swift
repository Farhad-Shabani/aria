import SwiftUI

struct PaletteCommandsPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var listen: ListenStatus
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
            commandList.confirmingDeletion(of: $controller.deletion, from: history)
        } bottom: {
            PaletteFooter {
                footerNote
            } actions: {
                if controller.model.renamingID != nil {
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
                "All saved dictations, Mac Audio sessions, and asks will be deleted, with their recordings "
                    + "and transcript files. This can’t be undone. Your history setting won’t change."
            )
        }
        .announcingNotes(of: history)
        .onChange(of: controller.model.promptFailure) { _, failure in
            if let failure { NSAccessibility.announce(failure.message) }
        }
        .onChange(of: listen.didFailToStart) { _, didFail in
            guard didFail else { return }
            NSAccessibility.announce(ListenStatus.startFailure)
        }
        .announcingStopFailure(of: listen)
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
                                    if let entry = item.entry { history.loadMore(after: entry.id) }
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
                listen: listen, isSelected: isSelected,
                show: { controller.run(command) }, stop: { controller.stopTranscription() })
        case .command(let command):
            PaletteCommandRow(command: command, isSelected: isSelected) { controller.run(command) }
        case .entry(let entry) where entry.id == controller.model.renamingID:
            PaletteEntryRow(
                entry: entry, isSelected: isSelected,
                titleField: PaletteTitleField(controller: controller, placeholder: entry.preview)
            ) {}
        case .entry(let entry):
            PaletteEntryRow(entry: entry, isSelected: isSelected, badge: badge(for: entry)) {
                controller.choose(item)
            }
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
        if listen.isStarting {
            PaletteFooterProgress("Starting…")
        } else if listen.didFailToStart {
            NoticeText(notice: ListenStatus.startFailure)
        } else if listen.didFailToStop {
            NoticeText(notice: ListenStatus.stopFailure)
        } else if let failure = controller.model.promptFailure {
            NoticeText(notice: failure.message)
        } else if controller.model.keepsHistory, let error = history.error {
            NoticeText(notice: error.message)
        } else if controller.model.keepsHistory, history.isClearing {
            PaletteFooterProgress("Clearing…")
        } else if controller.model.keepsHistory, let notice = history.notice {
            Text(notice)
        } else if controller.model.setupSteps > 0 {
            // A note on what was just done comes first. Searching… only flickers by.
            FinishSetupButton(steps: controller.model.setupSteps) { controller.run(.finishSetup) }
        } else if !controller.model.keepsHistory {
            Text("Nothing is saved.")
        } else if history.isLoading {
            PaletteFooterProgress("Searching…")
        }
    }

}

/// Finish Setting Up Aria, as the menu bar menu offers it, while steps are left.
private struct FinishSetupButton: View {
    let steps: Int
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text("Finish Setting Up")
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
