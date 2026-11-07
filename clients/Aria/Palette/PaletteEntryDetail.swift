import SwiftUI

/// One saved entry in full, then the prompt results run on it, as a conversation reads.
struct PaletteEntryDetail: View {
    /// One scroll target per block, so the keys bring the current one into view.
    private enum Block: Hashable {
        case entry
        case result(UInt64)
    }

    let entry: HistoryItem
    /// Oldest first.
    let thread: [PromptResult]
    /// The result the keys rest on. `nil` rests on the entry. Nothing is marked while there is no
    /// thread to move through.
    let focus: PromptResult.ID?
    let copy: (PromptResult) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var focusedBlock: Block? {
        guard !thread.isEmpty else { return nil }
        return focus.map(Block.result) ?? .entry
    }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    explanation
                    if let warning = entry.warning {
                        Text(warning).foregroundStyle(.secondary)
                    }
                    content
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .modifier(PaletteBlockHighlight(isFocused: focusedBlock == .entry))
                        .id(Block.entry)
                    ForEach(thread) { result in
                        PromptResultView(result: result) { copy(result) }
                            .modifier(PaletteBlockHighlight(isFocused: focusedBlock == .result(result.id)))
                            .id(Block.result(result.id))
                    }
                }
                .padding(.horizontal, PageLayout.textInset - PaletteBlockHighlight.inset)
                .padding(.vertical, PageLayout.verticalInset - PaletteBlockHighlight.inset)
            }
            // A thread arriving marks the entry without moving the reader.
            .onChange(of: focusedBlock) { old, block in
                guard old != nil, let block else { return }
                withAnimation(reduceMotion ? nil : .paletteQuick) { proxy.scrollTo(block) }
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        switch entry.kind {
        case .dictation:
            Text(entry.text)
        case .listen(let transcript, let read):
            if let note = Self.note(transcript: transcript, read: read) {
                Text(note).foregroundStyle(.secondary)
            }
            Text(entry.text)
        case .ask(let reply):
            VStack(alignment: .leading, spacing: 12) {
                Text(entry.text).font(.headline)
                Text(reply)
            }
        }
    }

    private static func note(transcript: String, read: HistoryItem.TranscriptRead?) -> String? {
        let file = URL(fileURLWithPath: transcript).lastPathComponent
        return switch read {
        case .truncated: "This is the first 1 MB. The whole transcript is in \(file)."
        case .unavailable: "The transcript file is gone or can’t be read, so this is only its start."
        case .whole, nil: nil
        }
    }

    @ViewBuilder
    private var explanation: some View {
        switch entry.status {
        case .transcriptionFailed:
            Label("Couldn’t Transcribe", systemImage: "exclamationmark.triangle")
                .font(.headline)
            Text(entry.message ?? "Aria couldn’t finish transcribing this recording.")
                .foregroundStyle(.secondary)
            if let note = entry.audioNote {
                Text(note).foregroundStyle(.secondary)
            }
            if !entry.text.isEmpty {
                Text("Incomplete transcript").font(.caption).foregroundStyle(.secondary)
            }
        case .deliveryFailed:
            Text("Aria couldn’t type the text. You can copy it below.")
                .foregroundStyle(.secondary)
        case .deletionPending:
            Text("Deletion is pending. Aria will finish removing this entry and its recording.")
                .foregroundStyle(.secondary)
        case .success, .recovered:
            EmptyView()
        }
    }
}
