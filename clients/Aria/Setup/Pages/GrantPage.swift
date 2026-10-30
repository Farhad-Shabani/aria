import SwiftUI

struct GrantPage: View {
    var body: some View {
        Form {
            Section {
                LabeledContent {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Waiting").foregroundStyle(.secondary)
                    }
                } label: {
                    Text("Accessibility")
                    Text("Turn on Aria in Privacy & Security → Accessibility.")
                }
            } footer: {
                Text("Setup continues on its own once Aria is on.")
            }
        }
    }
}
