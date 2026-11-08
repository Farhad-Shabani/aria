import SwiftUI

struct PaletteLiveRow: View {
    private static let recordSide: CGFloat = 22
    /// The glyph sits inside its frame, so the button reaches past the row's
    /// padding to end where other rows' trailing text does.
    private static let recordGlyphInset: CGFloat = 3

    @ObservedObject var systemTranscription: SystemTranscriptionStatus
    let isSelected: Bool
    let show: () -> Void
    let stop: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isHoveringRecord = false

    var body: some View {
        PaletteRow(symbol: "waveform", isSelected: isSelected, isSymbolAnimating: true, action: show) {
            HStack(spacing: 10) {
                heard
                SystemTranscriptionClock(systemTranscription).paletteRowDetail()
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Live Transcript")
            .accessibilityHint("Shows the transcript.")
        } accessory: {
            recordButton.padding(.trailing, -Self.recordGlyphInset)
        }
    }

    private var heard: some View {
        let words = systemTranscription.transcript.lines.last?.text ?? "Live Transcript"
        return ZStack(alignment: .leading) {
            Text(words)
                .paletteRowText(truncation: .head)
                .id(words)
                .transition(reduceMotion ? .identity : .push(from: .bottom))
        }
        .clipped()
        .animation(reduceMotion ? nil : .smooth, value: words)
    }

    /// Red while recording, as a recorder's is. Hovering turns it into a stop symbol.
    @ViewBuilder
    private var recordButton: some View {
        if systemTranscription.isStopping {
            ProgressView().controlSize(.small).frame(width: Self.recordSide, height: Self.recordSide)
                .accessibilityLabel("Stopping")
        } else {
            Button(action: stop) {
                Image(systemName: isHoveringRecord ? "stop.circle.fill" : "record.circle")
                    .font(.title3)
                    .foregroundStyle(.red)
                    .symbolEffect(.pulse, options: .repeating, isActive: !reduceMotion && !isHoveringRecord)
                    .frame(width: Self.recordSide, height: Self.recordSide)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .onHover { isHoveringRecord = $0 }
            .help("Stop Transcribing (⌘.)")
            .accessibilityLabel("Stop Transcribing")
        }
    }
}
