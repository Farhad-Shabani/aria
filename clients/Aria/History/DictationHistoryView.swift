import AppKit
import SwiftUI

struct DictationHistoryView: View {
    @StateObject private var state = DictationHistoryState(effects: DictationHistoryClient.effects)
    @State private var deletion: HistoryItem?
    @State private var isConfirmingClear = false
    @State private var isVisible = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField(
                    "Search transcriptions…",
                    text: Binding(get: { state.query }, set: { state.search($0) })
                )
                .textFieldStyle(.plain)
                Button {
                    state.refresh()
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .help("Refresh history")
                .accessibilityLabel("Refresh history")
            }
            .disabled(state.isClearing)
            .padding()
            Divider()
            HSplitView {
                historyList.frame(minWidth: 200, idealWidth: 260, maxWidth: 340)
                detail.frame(minWidth: 340, maxWidth: .infinity, maxHeight: .infinity)
            }
            .disabled(state.isClearing)
            if let error = state.error {
                Text(error.message)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal)
                    .padding(.vertical, 8)
            }
            if let notice = state.notice {
                Text(notice).foregroundStyle(.secondary).padding(.bottom, 8)
            }
            Divider()
            footer
        }
        .frame(minWidth: 620, minHeight: 420)
        .onAppear {
            isVisible = true
            NSApplication.shared.activate()
            state.refresh()
        }
        .onDisappear {
            isVisible = false
            state.close()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) {
            if ($0.object as? NSWindow)?.title == "Dictation History" {
                isVisible = true
                state.refresh()
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.willCloseNotification)) {
            if ($0.object as? NSWindow)?.title == "Dictation History" {
                isVisible = false
                state.close()
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .dictationHistoryChanged)) { _ in
            if isVisible { state.refresh() }
        }
        .confirmationDialog(
            "Delete this dictation?",
            isPresented: Binding(get: { deletion != nil }, set: { if !$0 { deletion = nil } }),
            titleVisibility: .visible,
            presenting: deletion
        ) { entry in
            Button("Delete", role: .destructive) { state.delete(entry.id) }
            Button("Cancel", role: .cancel) {}
        } message: { entry in
            let suffix =
                if case .available = entry.audio { " along with its saved recording." } else { "." }
            Text(
                "“\(String(entry.preview.prefix(80)))” will be deleted" + suffix
            )
        }
        .confirmationDialog(
            "Clear all dictation history?", isPresented: $isConfirmingClear, titleVisibility: .visible
        ) {
            Button("Clear History", role: .destructive) { state.clear() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "All saved dictations and their retained recordings will be deleted. This cannot be undone. "
                    + "Your history saving setting will not change."
            )
        }
    }

    @ViewBuilder
    private var historyList: some View {
        if state.items.isEmpty {
            VStack(spacing: 10) {
                if state.isLoading {
                    ProgressView("Loading history…")
                } else if let error = state.error {
                    ContentUnavailableView(
                        error == .offline ? "Aria unavailable" : "History unavailable",
                        systemImage: "exclamationmark.triangle",
                        description: Text(error.message)
                    )
                } else {
                    ContentUnavailableView(
                        state.query.isEmpty ? "No dictation history" : "No matches",
                        systemImage: "text.bubble",
                        description: Text(
                            state.query.isEmpty
                                ? "Your saved dictations will appear here." : "Try a different search."
                        )
                    )
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List(selection: Binding(get: { state.selectedID }, set: { state.select($0) })) {
                ForEach(state.days, id: \.start) { day in
                    Section(dayTitle(day.start)) {
                        ForEach(day.items) { entry in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(entry.preview).lineLimit(2)
                                if entry.status != .success && entry.status != .recovered {
                                    Text(entry.status.title).font(.caption).foregroundStyle(.secondary)
                                }
                            }
                            .padding(.vertical, 4)
                            .tag(entry.id)
                            .onAppear { state.loadMore(after: entry.id) }
                        }
                    }
                }
                if state.isLoading { ProgressView().frame(maxWidth: .infinity) }
            }
            .listStyle(.sidebar)
        }
    }

    @ViewBuilder
    private var detail: some View {
        if state.isLoadingDetail {
            ProgressView("Loading dictation…")
        } else if let entry = state.detail {
            VStack(alignment: .leading, spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: 12) {
                        if entry.status == .transcriptionFailed {
                            Label("Transcription failed", systemImage: "exclamationmark.triangle")
                                .font(.headline)
                            Text(entry.message ?? "Aria could not finish transcribing this recording.")
                                .foregroundStyle(.secondary)
                            if !entry.text.isEmpty {
                                Text("Incomplete transcript").font(.caption).foregroundStyle(.secondary)
                            }
                        } else if entry.status == .deliveryFailed {
                            Text("The text could not be inserted. You can copy it below.")
                                .foregroundStyle(.secondary)
                        } else if entry.status == .deletionPending {
                            Text("Deletion is pending. Aria will finish removing this entry and its recording.")
                                .foregroundStyle(.secondary)
                        }
                        if let warning = entry.warning {
                            Text(warning).foregroundStyle(.secondary)
                        }
                        Text(entry.text)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .padding(24)
                }
                VStack(spacing: 0) {
                    ForEach(Array(entry.metadata.enumerated()), id: \.offset) { index, row in
                        HStack(alignment: .firstTextBaseline) {
                            Text(row.0).foregroundStyle(.secondary)
                            Spacer(minLength: 16)
                            Text(row.1).multilineTextAlignment(.trailing).textSelection(.enabled)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                        .background(index.isMultiple(of: 2) ? Color.primary.opacity(0.04) : .clear)
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                    }
                }
                .font(.callout)
                .padding(16)
            }
        } else {
            Text("Select a dictation").foregroundStyle(.secondary)
        }
    }

    private var footer: some View {
        HStack {
            Button(state.isClearing ? "Clearing…" : "Clear History…", role: .destructive) { isConfirmingClear = true }
                .disabled(!state.canClear)
            Spacer()
            if let entry = state.detail {
                Button("Delete…", role: .destructive) { deletion = entry }
                    .disabled(
                        state.isClearing || entry.isRetrying || state.retryingID == entry.id || state.deletingID != nil)
                let isRetrying = entry.isRetrying || state.retryingID == entry.id
                if !entry.text.isEmpty && entry.status != .deletionPending {
                    Button("Copy text") { state.copy() }
                        .keyboardShortcut("c", modifiers: [.command, .shift])
                        .disabled(state.isClearing)
                }
                if entry.isRetryable || isRetrying {
                    Button(isRetrying ? "Retrying…" : "Retry transcription") { state.retry() }
                        .buttonStyle(.borderedProminent)
                        .disabled(
                            state.isClearing || entry.isRetrying || state.retryingID != nil
                                || state.deletingID == entry.id)
                }
            }
        }
        .padding(12)
    }

    private func dayTitle(_ date: Date) -> String {
        if Calendar.current.isDateInToday(date) { return "Today" }
        if Calendar.current.isDateInYesterday(date) { return "Yesterday" }
        return date.formatted(date: .abbreviated, time: .omitted)
    }
}
