import SwiftUI

struct PhoneSettings: View {
    @ObservedObject var pairing: PhonePairing
    @State private var isConfirmingRevoke = false

    var body: some View {
        Group {
            if pairing.status == .daemonOff {
                DaemonWaitingView(isLoading: false, description: "These settings load once Aria is running.")
            } else {
                form
            }
        }
        .task { await pairing.run() }
    }

    private var form: some View {
        Form {
            Section {
                Toggle(
                    isOn: Binding(
                        get: { pairing.isOn },
                        set: { on in Task { await pairing.turn(on: on) } }
                    )
                ) {
                    Text("Phone pairing")
                    if pairing.status == .restarting {
                        HStack(spacing: 6) {
                            ProgressView().controlSize(.small)
                            Text("Restarting Aria…")
                        }
                    }
                }
                .disabled(!pairing.canSwitch)
            } footer: {
                problems
            }
            if case .live(let invite, let expiresAt) = pairing.status {
                Section {
                    live(invite, expiresAt: expiresAt)
                }
            }
        }
    }

    @ViewBuilder
    private var problems: some View {
        switch pairing.status {
        case .failed:
            NoticeText(notice: PhonePairing.codeFailure)
        case .loading, .restarting, .relayOff, .daemonOff, .live:
            EmptyView()
        }
        NoticeText(notice: pairing.actionError)
    }

    private func live(_ invite: PairingInvite, expiresAt: Date) -> some View {
        PairingCodeCard(invite: invite, expiresAt: expiresAt, phonePairedAt: pairing.phonePairedAt) {
            actions(invite, expiresAt: expiresAt)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 8)
        .confirmationDialog("Revoke Every Paired Phone?", isPresented: $isConfirmingRevoke) {
            Button("Revoke All", role: .destructive) {
                Task { await pairing.revoke() }
            }
        } message: {
            Text("Each phone scans a new code to pair again.")
        }
    }

    private func actions(_ invite: PairingInvite, expiresAt: Date) -> some View {
        HStack(spacing: 8) {
            Button {
                pairing.copy(invite, until: expiresAt)
            } label: {
                Text("Copy Code").frame(maxWidth: .infinity)
            }
            Button(role: .destructive) {
                isConfirmingRevoke = true
            } label: {
                Text("Revoke All…").frame(maxWidth: .infinity)
            }
            .disabled(!pairing.canRevoke)
        }
    }
}
