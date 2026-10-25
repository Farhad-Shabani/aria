import SwiftUI

struct TranscriptionSection: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var server: ServerTranscription
    @ObservedObject var setup: SetupStatus
    @State private var pairing = false
    @State private var forgetting = false

    var body: some View {
        Section {
            Picker("Runs on", selection: location) {
                Text("This Mac").tag(TranscriptionLocation.thisMac)
                Text("My server").tag(TranscriptionLocation.server)
            }
            .disabled(!server.canChooseLocation)
            serverRow
        } header: {
            Text("Transcription")
        } footer: {
            footer
        }
        .sheet(isPresented: $pairing) {
            PairServerSheet(server: server, runsOnServer: setup.transcription == .server, isPresented: $pairing)
        }
        .confirmationDialog("Forget this server?", isPresented: $forgetting, presenting: setup.server) { paired in
            Button("Copy Revoke Command and Forget", role: .destructive) {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString("aria remote revoke \(paired.clientFingerprint)", forType: .string)
                Task { await server.forget() }
            }
            Button("Forget", role: .destructive) { Task { await server.forget() } }
        } message: { _ in
            Text("The server keeps trusting this Mac until you run the revoke command there.")
        }
    }

    private var location: Binding<TranscriptionLocation> {
        Binding(
            get: { setup.transcription },
            set: { location in Task { await server.use(location) } }
        )
    }

    private var serverRow: some View {
        LabeledContent {
            HStack {
                if setup.server != nil {
                    Button("Forget…") { forgetting = true }
                        .disabled(!server.canForget)
                        .help(setup.transcription == .server ? "Set Runs on to This Mac first." : "")
                }
                Button(setup.server == nil ? "Pair…" : "Pair Again…") {
                    server.beginPairing()
                    pairing = true
                }
                .disabled(server.busy)
            }
        } label: {
            Text("Server")
            if let paired = setup.server {
                Text(ServerTranscription.address(of: paired.endpoint) ?? paired.endpoint)
                Text("This Mac: \(paired.clientFingerprint.prefix(4))…\(paired.clientFingerprint.suffix(4))")
                    .help(paired.clientFingerprint)
                    .contextMenu {
                        Button("Copy Fingerprint") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(paired.clientFingerprint, forType: .string)
                        }
                    }
            }
        }
    }

    @ViewBuilder
    private var footer: some View {
        if server.busy {
            Text("Updating Aria…")
        } else {
            switch server.notice {
            case .failed(let message):
                Text(message).foregroundStyle(.red)
            case .needsSetup:
                HStack {
                    Text("No speech model is on this Mac yet.").foregroundStyle(.red)
                    Button("Aria Setup…") { openWindow(id: WindowID.setup) }
                }
            case nil:
                Text("Audio goes to the server only when Runs on is My server.")
            }
        }
    }
}

private struct PairServerSheet: View {
    private static let width: CGFloat = 420
    private static let glyphSide: CGFloat = 56
    private static let inviteCommand = "aria remote invite --endpoint https://your-server:9851"

    @ObservedObject var server: ServerTranscription
    let runsOnServer: Bool
    @Binding var isPresented: Bool

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "server.rack")
                .font(.title)
                .foregroundStyle(.white)
                .frame(width: Self.glyphSide, height: Self.glyphSide)
                .background(Circle().fill(.blue))
                .accessibilityHidden(true)
            Text("Pair with a Server").font(.headline)
            if case .confirming(_, let address) = server.pairingStep {
                confirmation(address)
            } else {
                instructions
            }
            if let failure = server.pairingStep == .unreadable
                ? "The clipboard holds no invitation." : server.pairingFailure
            {
                Text(failure)
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { isPresented = false }
                    .keyboardShortcut(.cancelAction)
                if case .confirming(let invitation, _) = server.pairingStep {
                    Button("Pair") { Task { await pair(invitation) } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(server.busy)
                } else {
                    Button("Paste Invitation") { server.paste(NSPasteboard.general.string(forType: .string) ?? "") }
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
        .padding(24)
        .frame(width: Self.width)
    }

    private var instructions: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("On the server, run:")
            Text(Self.inviteCommand)
                .font(.body.monospaced())
                .textSelection(.enabled)
            Text("Copy the invitation it prints. It works once, within 60 seconds.")
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func confirmation(_ address: String) -> some View {
        VStack(spacing: 6) {
            Text(address).font(.body.monospaced())
            Text(runsOnServer ? "Dictation audio will go to this server." : "Pairing alone sends no audio.")
                .foregroundStyle(.secondary)
        }
    }

    /// The invitation is a secret; once spent, it leaves the clipboard.
    private func pair(_ invitation: String) async {
        guard await server.pair(invitation: invitation) else { return }
        if NSPasteboard.general.string(forType: .string) == invitation { NSPasteboard.general.clearContents() }
        isPresented = false
    }
}
