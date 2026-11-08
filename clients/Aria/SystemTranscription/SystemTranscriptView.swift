import SwiftUI

/// The lines of a system transcription, following the newest one while the
/// reader stays at the end.
struct SystemTranscriptView: View {
    private struct ScrollPosition: Equatable {
        let offset: CGFloat
        let isAtEnd: Bool
    }

    private static let stampWidth: CGFloat = 64
    /// How near the end still counts as at the end.
    private static let endSlack: CGFloat = 8

    let transcript: SystemTranscript
    let isRunning: Bool
    @State private var isPinnedToEnd = true

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(transcript.lines) { line in
                        row(line).id(line.id)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, PageLayout.textInset)
                .padding(.vertical, PageLayout.verticalInset)
            }
            .overlay {
                if transcript.lines.isEmpty {
                    ContentUnavailableView(
                        isRunning ? "Transcribing Your Mac’s Audio…" : "Nothing Transcribed", systemImage: "waveform")
                }
            }
            .onScrollGeometryChange(for: ScrollPosition.self) { geometry in
                ScrollPosition(
                    offset: geometry.contentOffset.y,
                    isAtEnd: geometry.visibleRect.maxY >= geometry.contentSize.height - Self.endSlack)
            } action: { old, new in
                // Only the reader's own scrolling moves the pin. Lines arriving
                // under a still view leave it where it was.
                if new.offset != old.offset { isPinnedToEnd = new.isAtEnd }
            }
            .onAppear { scrollToEnd(proxy) }
            // The last line also grows as words join it.
            .onChange(of: transcript.lines.last) {
                if isPinnedToEnd { scrollToEnd(proxy) }
            }
        }
        .textSelection(.enabled)
    }

    private func row(_ line: SystemTranscript.Line) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(SystemTranscript.stamp(line.offsetSeconds))
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: Self.stampWidth, alignment: .leading)
            VStack(alignment: .leading, spacing: 2) {
                if !line.speakerLabel.isEmpty {
                    Text(line.speakerLabel)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                }
                Text(line.text)
            }
        }
        .accessibilityElement(children: .combine)
    }

    private func scrollToEnd(_ proxy: ScrollViewProxy) {
        guard let last = transcript.lines.last?.id else { return }
        proxy.scrollTo(last, anchor: .bottom)
    }
}
