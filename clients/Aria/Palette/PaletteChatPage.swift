import SwiftUI

struct PaletteChatPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var chats: Chats
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            header
        } content: {
            if let chat = chats.shown {
                PaletteChatDetail(
                    chat: chat,
                    selection: controller.model.messageSelection,
                    assistantName: controller.assistantName(of: chat.key),
                    readFailed: chats.error != nil,
                    readEarlier: chats.readEarlier,
                    retryRead: chats.retryRead,
                    openEntry: controller.openEntry(of:))
            }
        } bottom: {
            VStack(spacing: 0) {
                followUp
                PaletteFooter {
                    footerNote
                } actions: {
                    if controller.model.isRenaming {
                        PaletteTitleActions(controller: controller)
                    } else {
                        let actions = controller.actions
                        PaletteFooterActions(
                            actions: actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
                        if let title = controller.returnTitle {
                            if !actions.isEmpty { BarDivider() }
                            PaletteFooterButton(title: title, key: .return) { controller.returnOnFollowUp() }
                                .disabled(!controller.canReturnOnFollowUp)
                        }
                    }
                }
            }
        }
        .overlay(alignment: .bottomTrailing) { PaletteActionsMenu(controller: controller, history: history) }
        .confirmingDeletion(of: $controller.chatDeletion, perform: controller.deleteChat)
        .announcingNotes(of: history)
        .onChange(of: chats.shown?.asks.last?.state) { old, state in
            guard case .running? = old else { return }
            switch state {
            case .replied?: NSAccessibility.announce("Reply ready")
            case .failed(let message)?: NSAccessibility.announce(message)
            case .lost?: NSAccessibility.announce(ChatAsk.lost)
            case .running?, nil: break
            }
        }
    }

    private var header: some View {
        PaletteHeader(back: controller.escape) {
            let shown = chats.shown
            let question = shown?.asks.first?.question ?? "Chat"
            if let key = shown?.key, controller.model.renaming == .chat(key) {
                PaletteTitleField(controller: controller, placeholder: question, style: .headline)
            } else {
                Text(shown?.key?.name ?? question)
            }
        } detail: {
            Text(place)
        }
    }

    private var place: String {
        let shown = chats.shown
        let folder = shown?.folder.map { URL(fileURLWithPath: $0).lastPathComponent }
        return [controller.assistantName(of: shown?.key), folder].compactMap(\.self).joined(separator: " · ")
    }

    private var followUp: some View {
        PaletteFollowUpRow(
            // Off while a name is edited, so the name field keeps focus.
            controller: controller, isEnabled: chats.takesFollowUp && !controller.model.isRenaming,
            placeholder: chats.shown?.isReplying == true ? "Replying…" : "Ask a follow-up")
    }

    @ViewBuilder
    private var footerNote: some View {
        if let failure = chats.stopFailure {
            NoticeText(notice: failure)
        } else if let error = chats.error {
            NoticeText(notice: error.message)
        } else if let error = history.error {
            NoticeText(notice: error.message)
        } else if let notice = history.notice {
            Text(notice)
        } else if !controller.model.keepsHistory {
            Text("Nothing is saved.")
        }
    }
}

private enum Metrics {
    /// The asks' text starts where a row's text does and ends as far from the right edge.
    static let textInset =
        PaletteBarMetrics.leading + PaletteRowMetrics.symbolWidth + PaletteRowMetrics.columnSpacing
    /// Between one ask's highlight and the next's, so each reads as its own exchange.
    static let askGap: CGFloat = 14
    /// Between a question's highlight and its reply's, so neither covers the other.
    static let replyGap: CGFloat = 2
    static let replyLineSpacing: CGFloat = 3
    /// How far a question's bubble stays from the reply side, so it reads as the person's.
    static let bubbleIndent: CGFloat = 160
    static let bubbleRadius: CGFloat = 12
    static let bubblePadding = EdgeInsets(top: 7, leading: 12, bottom: 7, trailing: 12)
    /// Enough to recognize the text. Its own page shows all of it.
    static let quoteLines = 6
    static let quoteIndent: CGFloat = 10
    static let quoteBarWidth: CGFloat = 3
}

private struct PaletteChatDetail: View {
    let chat: Chat
    let selection: MessageSelection
    let assistantName: String?
    /// The footer shows why the read failed, so the list offers Try Again with no message.
    let readFailed: Bool
    let readEarlier: () -> Void
    let retryRead: () -> Void
    let openEntry: (ChatAsk) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let selected = Set(selection.selected(in: chat.asks))
        let focus = selection.focused(in: chat.asks)
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: Metrics.askGap) {
                    if let quote = chat.quote {
                        PaletteQuoteView(quote: quote)
                            .padding(.horizontal, Metrics.textInset - PaletteBarMetrics.listInset)
                    }
                    if readFailed {
                        Button("Try Again", action: retryRead)
                            .frame(maxWidth: .infinity)
                    } else if let older = chat.older {
                        // Keyed on the cursor, so the next page loads too while the spinner stays in view.
                        ProgressView()
                            .controlSize(.small)
                            .frame(maxWidth: .infinity)
                            .task(id: older) { readEarlier() }
                    }
                    ForEach(chat.asks) { ask in
                        VStack(alignment: .leading, spacing: Metrics.replyGap) {
                            message(ask, .question, selected: selected) {
                                PaletteQuestionView(question: ask.question)
                            }
                            message(ask, .reply, selected: selected) {
                                PaletteReplyView(ask: ask, assistantName: assistantName) { openEntry(ask) }
                            }
                        }
                    }
                }
                .padding(.horizontal, PaletteBarMetrics.listInset)
                .padding(.vertical, PageLayout.verticalInset - PaletteBlockHighlight.inset)
                .textSelection(.enabled)
            }
            .defaultScrollAnchor(.bottom)
            .onChange(of: focus) { _, focus in
                guard let focus, !selection.isResting else { return }
                withAnimation(reduceMotion ? nil : .paletteQuick) { proxy.scrollTo(focus) }
            }
            .onChange(of: chat.asks.last?.id) { _, last in
                guard let last else { return }
                let reply = ChatMessage(ask: last, side: .reply)
                withAnimation(reduceMotion ? nil : .paletteQuick) { proxy.scrollTo(reply, anchor: .bottom) }
            }
        }
    }

    /// Highlighted across the width a selected row takes. A selection resting on the newest reply
    /// shows no highlight, so a page reads clean until the keys move.
    private func message(
        _ ask: ChatAsk, _ side: ChatMessage.Side, selected: Set<ChatMessage>,
        @ViewBuilder content: () -> some View
    ) -> some View {
        let message = ChatMessage(ask: ask.id, side: side)
        return content()
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, Metrics.textInset - PaletteBarMetrics.listInset - PaletteBlockHighlight.inset)
            .modifier(PaletteBlockHighlight(isFocused: !selection.isResting && selected.contains(message)))
            .id(message)
    }
}

/// A chat's quote, above its asks, as a quote shows in Mail.
private struct PaletteQuoteView: View {
    let quote: String

    var body: some View {
        Text(quote)
            .foregroundStyle(.secondary)
            .lineLimit(Metrics.quoteLines)
            .padding(.leading, Metrics.quoteIndent)
            .overlay(alignment: .leading) {
                Capsule().fill(.tertiary).frame(width: Metrics.quoteBarWidth)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Quoted: \(quote)")
    }
}

private struct PaletteQuestionView: View {
    let question: String

    var body: some View {
        HStack(spacing: 0) {
            Spacer(minLength: Metrics.bubbleIndent)
            Text(question)
                .padding(Metrics.bubblePadding)
                .background(.quinary, in: RoundedRectangle(cornerRadius: Metrics.bubbleRadius, style: .continuous))
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("You asked: \(question)")
    }
}

private struct PaletteReplyView: View {
    let ask: ChatAsk
    let assistantName: String?
    let showResults: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            answer
                // One element, so VoiceOver reads only the label, not the spinner and its step.
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(spokenAnswer)
            details
        }
    }

    @ViewBuilder
    private var answer: some View {
        switch ask.state {
        case .running(let step):
            HStack(spacing: 6) {
                ProgressView().controlSize(.mini)
                Text(step)
            }
            .font(.callout)
            .foregroundStyle(.secondary)
        case .replied(let reply):
            // Equatable on its text, so another ask's step changing doesn't parse the Markdown again.
            PaletteReplyText(reply: reply).equatable()
        case .failed(let message):
            NoticeText(notice: message)
        case .lost:
            NoticeText(notice: ChatAsk.lost)
        }
    }

    private var spokenAnswer: String {
        let text =
            switch ask.state {
            case .running(let step): step
            case .replied(let reply): reply
            case .failed(let message): message
            case .lost: ChatAsk.lost
            }
        return "\(assistantName.map { "\($0) replied" } ?? "Reply"): \(text)"
    }

    private var details: some View {
        HStack(spacing: 6) {
            Text(HistoryDay.time(of: ask.created))
            if ask.resultCount > 0 {
                Text("·").accessibilityHidden(true)
                Button(PromptResult.countLabel(ask.resultCount), action: showResults)
                    .buttonStyle(.borderless)
                    .help("Show Prompt Results")
            }
        }
        .font(.caption)
        .foregroundStyle(.tertiary)
    }
}

/// Assistants answer in Markdown. Inline marks show as styles, and line breaks stay.
private struct PaletteReplyText: View, Equatable {
    let reply: String

    var body: some View {
        Text(formatted)
            .lineSpacing(Metrics.replyLineSpacing)
            // A reply is untrusted: it can echo a project's files. Only a web link opens.
            .environment(
                \.openURL,
                OpenURLAction { url in
                    ["http", "https"].contains(url.scheme?.lowercased()) ? .systemAction : .discarded
                })
    }

    private var formatted: AttributedString {
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: reply, options: options)) ?? AttributedString(reply)
    }
}
