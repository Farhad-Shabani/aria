import SwiftUI

struct TranscriptionSection: View {
    private static let rowTitle = "Transcribe on your server"

    @Environment(\.openWindow) private var openWindow
    @ObservedObject var server: ServerTranscription
    @ObservedObject var setup: SetupStatus
    @State private var isPairing = false
    @State private var isConfirmingForget = false

    var body: some View {
        Section {
            serverRow
            if server.notice == .needsSetup {
                LabeledContent {
                    Button("Aria Setup…") { openWindow(id: WindowID.setup) }
                } label: {
                    Text("Speech model")
                    Text("No speech model is on your Mac yet.")
                }
            }
        } footer: {
            footer
        }
        .sheet(isPresented: $isPairing) {
            PairServerSheet(server: server, isPresented: $isPairing)
        }
        .confirmationDialog(
            "Forget This Server?", isPresented: $isConfirmingForget, presenting: setup.server
        ) { paired in
            Button("Copy Revoke Command and Forget", role: .destructive) {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString("aria remote revoke \(paired.clientFingerprint)", forType: .string)
                Task { await server.forget() }
            }
            Button("Forget", role: .destructive) { Task { await server.forget() } }
        } message: { _ in
            Text("The server keeps trusting your Mac until you run the revoke command there.")
        }
    }

    private var usesServer: Binding<Bool> {
        Binding(
            get: { setup.transcription == .server },
            set: { isOn in Task { await server.use(isOn ? .server : .thisMac) } }
        )
    }

    @ViewBuilder
    private var serverRow: some View {
        if server.needsPairing {
            LabeledContent(Self.rowTitle) {
                Button("Pair…") {
                    server.beginPairing()
                    isPairing = true
                }
                .disabled(server.isBusy)
            }
        } else {
            LabeledContent {
                HStack {
                    if setup.server != nil {
                        Button("Forget…", role: .destructive) { isConfirmingForget = true }
                            .disabled(!server.canForget)
                            .help(setup.transcription == .server ? "Turn off \(Self.rowTitle) first." : "")
                    }
                    Toggle(Self.rowTitle, isOn: usesServer)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .disabled(!server.canChooseLocation)
                }
            } label: {
                Text(Self.rowTitle)
                if let paired = setup.server {
                    Text(ServerTranscription.address(of: paired.endpoint) ?? paired.endpoint)
                }
            }
        }
    }

    @ViewBuilder
    private var footer: some View {
        if server.isBusy {
            HStack {
                ProgressView().controlSize(.small)
                Text("Updating Aria…")
            }
        } else {
            switch server.notice {
            case .failed(let message):
                Text(message).foregroundStyle(.failure)
            case .needsSetup, nil:
                Text("Recordings stay on your Mac unless you pair a server and turn this on.")
            }
        }
    }
}

private struct PairServerSheet: View {
    private static let width: CGFloat = 420
    private static let inviteCommand = "aria remote invite --endpoint https://your-server:9851"

    @ObservedObject var server: ServerTranscription
    @Binding var isPresented: Bool

    var body: some View {
        VStack(spacing: SettingsMetrics.sheetSpacing) {
            SheetGlyph(symbol: "server.rack", tint: .blue)
            Text("Pair with a Server").font(.headline)
            if case .confirming(_, let address) = server.pairingStep {
                confirmation(address)
            } else {
                instructions
            }
            if let failure = server.pairingStep == .unreadable
                ? "The clipboard doesn’t hold an invitation. Copy it, then click **Paste Invitation**."
                : server.pairingFailure
            {
                Text(markdown: failure)
                    .foregroundStyle(.failure)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(SettingsMetrics.sheetPadding)
        .frame(width: Self.width)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel", role: .cancel) { isPresented = false }
                    .keyboardShortcut(.cancelAction)
            }
            ToolbarItem(placement: .confirmationAction) {
                if case .confirming(let invitation, _) = server.pairingStep {
                    Button("Pair") { Task { await pair(invitation) } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(server.isBusy)
                } else {
                    Button("Paste Invitation") { server.paste(NSPasteboard.general.string(forType: .string) ?? "") }
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
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
            Text("Pairing alone sends no audio.")
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
