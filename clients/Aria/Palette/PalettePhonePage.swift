import SwiftUI

/// The code your iPhone scans to pair, as the Phone pane shows it.
struct PalettePhonePage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var pairing: PhonePairing

    var body: some View {
        PaletteBars {
            PaletteHeader(back: controller.escape) {
                Text(PaletteCommand.pairPhone.title)
            }
        } content: {
            content
        } bottom: {
            PaletteFooter {
                if pairing.status == .restarting {
                    PaletteFooterProgress("Restarting Aria…")
                } else {
                    NoticeText(notice: pairing.actionError)
                }
            } actions: {
                PaletteFooterActions(
                    actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
            }
        }
        .task { await pairing.run() }
        // The footer shows it, but focus stays put, so VoiceOver would miss it.
        .onChange(of: pairing.actionError) { _, error in
            if let error { NSAccessibility.announce(error) }
        }
    }

    @ViewBuilder
    private var content: some View {
        switch pairing.status {
        case .live(let invite, let expiresAt):
            PairingCodeCard(invite: invite, expiresAt: expiresAt, phonePairedAt: pairing.phonePairedAt)
        case .relayOff:
            ContentUnavailableView(
                "Phone Pairing Is Off", systemImage: "iphone.slash",
                description: Text("Turn it on to show a code your iPhone can scan."))
        case .daemonOff:
            DaemonWaitingView(isLoading: false, description: "The pairing code shows once Aria is running.")
        case .failed:
            ContentUnavailableView(
                "No Pairing Code", systemImage: "exclamationmark.triangle",
                description: Text(markdown: PhonePairing.codeFailure))
        case .loading, .restarting:
            ProgressView().controlSize(.small)
        }
    }
}
