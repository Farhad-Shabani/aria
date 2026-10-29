import SwiftUI

struct VoiceSection: View {
    private static let monogramSide: CGFloat = 28

    @Environment(\.openWindow) private var openWindow
    @ObservedObject var voices: VoiceProfiles
    @ObservedObject var setup: SetupStatus
    let editor: PreferencesEditor
    @State private var removing: Aria_Daemon_V1_EnrolledSpeaker?

    var body: some View {
        Section {
            ownVoiceOnly
                // On a row, not the form: the form first shows a waiting
                // view, and a task there would not run again once it loads.
                .task { await voices.refresh() }
            if let speakers = voices.speakers {
                profiles(speakers)
            }
        } header: {
            Text("Voice")
        } footer: {
            NoticeText(notice: voices.failure)
        }
        .sheet(isPresented: Binding(get: { voices.enrollment != nil }, set: { if !$0 { voices.cancel() } })) {
            if let enrollment = voices.enrollment {
                VoiceEnrollmentSheet(voices: voices, enrollment: enrollment)
            }
        }
        .confirmationDialog(
            "Remove \(removing?.name ?? "")’s voice?",
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            presenting: removing
        ) { speaker in
            Button("Remove", role: .destructive) { Task { await voices.remove(speaker.name) } }
        } message: { _ in
            Text("Aria won’t recognize this voice anymore.")
        }
    }

    @ViewBuilder
    private func profiles(_ speakers: [Aria_Daemon_V1_EnrolledSpeaker]) -> some View {
        if !setup.hasSpeakerModelOnDisk {
            LabeledContent {
                Button("Aria Setup…") { openWindow(id: WindowID.setup) }
            } label: {
                Text("Your Voice")
                Text("Needs the speaker verification model.")
            }
        } else if speakers.isEmpty {
            LabeledContent {
                Button("Set Up…") { voices.beginAdding() }
                    .buttonStyle(.borderedProminent)
            } label: {
                Text("Your Voice")
                Text("Teach Aria to tell you apart from others.")
            }
        } else {
            ForEach(speakers, id: \.name) { speaker in
                row(speaker)
            }
            HStack {
                Spacer()
                Button("Add Voice…") { voices.beginAdding() }
            }
        }
    }

    private func row(_ speaker: Aria_Daemon_V1_EnrolledSpeaker) -> some View {
        HStack(spacing: 10) {
            Text(speaker.name.prefix(1).uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(.white)
                .frame(width: Self.monogramSide, height: Self.monogramSide)
                .background(Circle().fill(.gray))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(speaker.name)
                Text(speaker.role == .owner ? "Owner" : "Guest")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Menu {
                if speaker.role != .owner {
                    Button("Make Owner") { Task { await voices.makeOwner(speaker.name) } }
                }
                Button("Set Up Again…") { voices.beginRerecording(speaker) }
                Button("Remove…", role: .destructive) { removing = speaker }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel("\(speaker.name) options")
        }
    }

    /// Stays switchable off when the owner's voice is gone, so a stale
    /// setting can still be cleared.
    private var ownVoiceOnly: some View {
        Toggle(isOn: editor.binding(\.ownVoiceOnly, \.ownVoiceOnly)) {
            Text("Only type my voice")
            Text(voices.ownVoiceSummary)
        }
        .disabled(!voices.hasOwner && !editor.snapshot.ownVoiceOnly)
    }
}
