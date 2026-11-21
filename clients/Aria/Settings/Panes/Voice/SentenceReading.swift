import SwiftUI

/// The sentence to read for the next sample, with a dot per sample.
struct SentenceReading: View {
    private static let dotSide: CGFloat = 8

    let sampling: VoiceSampling

    var body: some View {
        let step = min(sampling.taken, VoiceProfiles.samples - 1)
        VStack(spacing: SettingsMetrics.sheetSpacing) {
            Text("Sentence \(step + 1) of \(VoiceProfiles.samples)")
                .font(.subheadline)
                .foregroundStyle(.secondary)
            Text("“\(VoiceProfiles.sentences[step])”")
                .font(.title3.weight(.medium))
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .opacity(sampling.isListening ? 1 : 0.4)
            Group {
                if sampling.isListening {
                    Text("Read it aloud, then pause.")
                } else {
                    ProgressLabel("One moment…")
                }
            }
            .foregroundStyle(.secondary)
            HStack(spacing: 8) {
                ForEach(0..<VoiceProfiles.samples, id: \.self) { index in
                    dot(isDone: index < sampling.taken, isCurrent: sampling.isListening && index == sampling.taken)
                }
            }
            .accessibilityHidden(true)
        }
        .frame(maxWidth: .infinity)
    }

    private func dot(isDone: Bool, isCurrent: Bool) -> some View {
        Circle()
            .fill(isDone ? AnyShapeStyle(Color.voice) : AnyShapeStyle(.quaternary))
            .overlay { if isCurrent { Circle().strokeBorder(.voice, lineWidth: 2) } }
            .frame(width: Self.dotSide, height: Self.dotSide)
    }
}
