import SwiftUI

/// The phones your Mac pins, each with its own Revoke.
struct PairedPhonesSection: View {
    let phones: PhonePairing.PhoneList
    let canRevoke: Bool
    let revoke: (PairedPhone) -> Void
    @State private var confirming: PairedPhone?

    var body: some View {
        Section("Paired Phones") {
            switch phones {
            case .loading:
                ProgressView().controlSize(.small)
            case .failed:
                Text(PhonePairing.listFailure)
                    .foregroundStyle(.secondary)
            case .listed(let listed) where listed.isEmpty:
                Text("No phones are paired.")
                    .foregroundStyle(.secondary)
            case .listed(let listed):
                ForEach(listed) { phone in
                    row(phone)
                }
            }
        }
        .confirmationDialog(
            Text("Revoke “\(confirming?.name ?? "")”?"),
            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
            presenting: confirming
        ) { phone in
            Button("Revoke", role: .destructive) { revoke(phone) }
        } message: { _ in
            Text("It scans a new code to pair again.")
        }
    }

    private func row(_ phone: PairedPhone) -> some View {
        LabeledContent {
            Button("Revoke…", role: .destructive) { confirming = phone }
                .disabled(!canRevoke)
        } label: {
            Label {
                Text(phone.name)
                if let pairedAt = phone.pairedAt {
                    Text("Paired \(pairedAt.formatted(date: .abbreviated, time: .omitted))")
                }
            } icon: {
                Image(systemName: "iphone")
            }
        }
    }
}
