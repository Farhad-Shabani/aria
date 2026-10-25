import SwiftUI

struct VoiceEnrollmentSheet: View {
    private static let width: CGFloat = 380
    private static let glyphSide: CGFloat = 56
    private static let dotSide: CGFloat = 8

    @ObservedObject var voices: VoiceProfiles
    let enrollment: VoiceProfiles.Enrollment

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "waveform")
                .font(.title)
                .foregroundStyle(.white)
                .frame(width: Self.glyphSide, height: Self.glyphSide)
                .background(Circle().fill(.purple))
                .accessibilityHidden(true)
            switch enrollment.phase {
            case .naming: naming
            case .recording(let taken, let listening): recording(taken: taken, listening: listening)
            }
        }
        .padding(SettingsMetrics.sheetPadding)
        .frame(width: Self.width)
    }

    private var title: String {
        if let replacing = enrollment.replacing { return "Set Up \(replacing)’s Voice" }
        return enrollment.role == .owner ? "Set Up Your Voice" : "Add Voice"
    }

    @ViewBuilder
    private var naming: some View {
        Text(title).font(.headline)
        Text("You’ll read \(VoiceProfiles.samples) short sentences aloud. Your voice stays on this Mac.")
            .multilineTextAlignment(.center)
            .fixedSize(horizontal: false, vertical: true)
        if enrollment.replacing == nil {
            TextField("Name", text: $voices.name)
        }
        if let failure = enrollment.failure {
            Text(failure)
                .foregroundStyle(.red)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        HStack {
            Spacer()
            Button("Cancel", role: .cancel) { voices.cancel() }
                .keyboardShortcut(.cancelAction)
            Button("Start") { voices.start() }
                .keyboardShortcut(.defaultAction)
                .disabled(voices.name.trimmingCharacters(in: .whitespaces).isEmpty)
        }
    }

    @ViewBuilder
    private func recording(taken: Int, listening: Bool) -> some View {
        let step = min(taken, VoiceProfiles.samples - 1)
        Text("Sentence \(step + 1) of \(VoiceProfiles.samples)")
            .font(.subheadline)
            .foregroundStyle(.secondary)
        Text("“\(VoiceProfiles.sentences[step])”")
            .font(.title3.weight(.medium))
            .multilineTextAlignment(.center)
            .fixedSize(horizontal: false, vertical: true)
            .opacity(listening ? 1 : 0.4)
        Text(listening ? "Read it aloud, then pause." : "One moment…")
            .foregroundStyle(.secondary)
        HStack(spacing: 8) {
            ForEach(0..<VoiceProfiles.samples, id: \.self) { index in
                dot(done: index < taken, current: listening && index == taken)
            }
        }
        .accessibilityHidden(true)
        HStack {
            Spacer()
            Button("Cancel", role: .cancel) { voices.cancel() }
                .keyboardShortcut(.cancelAction)
        }
    }

    private func dot(done: Bool, current: Bool) -> some View {
        Circle()
            .fill(done ? AnyShapeStyle(.purple) : AnyShapeStyle(.quaternary))
            .overlay { if current { Circle().strokeBorder(.purple, lineWidth: 2) } }
            .frame(width: Self.dotSide, height: Self.dotSide)
    }
}
