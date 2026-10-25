import SwiftUI

struct GrantPage: View {
    var body: some View {
        Form {
            Section {
                Text("macOS asks to let Aria use Accessibility. Choose Open System Settings and switch on Aria.")
                Text("This page moves on by itself once the hotkey arms.")
                LabeledContent {
                    ProgressView().controlSize(.small)
                } label: {
                    Text("Waiting for the grant…").foregroundStyle(.secondary)
                }
            }
        }
    }
}
