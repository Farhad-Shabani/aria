import SwiftUI

struct VoiceEnrollmentSheet: View {
    private static let width: CGFloat = 380

    @ObservedObject var voices: VoiceProfiles
    let enrollment: VoiceProfiles.Enrollment

    var body: some View {
        VStack(spacing: SettingsMetrics.sheetSpacing) {
            SheetGlyph(symbol: "waveform", tint: .voice)
            switch enrollment.phase {
            case .naming: naming
            case .recording(let sampling): SentenceReading(sampling: sampling)
            }
        }
        .padding(SettingsMetrics.sheetPadding)
        .frame(width: Self.width)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel", role: .cancel) { voices.cancel() }
                    .keyboardShortcut(.cancelAction)
            }
            if case .naming = enrollment.phase {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Start") { voices.start() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(voices.name.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
    }

    private var title: String {
        if let replacing = enrollment.replacing { return "Set Up \(replacing)’s Voice" }
        return enrollment.isOwner ? "Set Up Your Voice" : "Add Voice"
    }

    @ViewBuilder
    private var naming: some View {
        Text(title).font(.headline)
        Text("You’ll read \(VoiceProfiles.samples) short sentences aloud. Your voice stays on your Mac.")
            .multilineTextAlignment(.center)
            .fixedSize(horizontal: false, vertical: true)
        if enrollment.replacing == nil {
            TextField("Name", text: $voices.name)
        }
        if let failure = enrollment.failure {
            Text(failure)
                .foregroundStyle(.failure)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}
