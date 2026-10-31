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
                    Text(paneName)
                    Text("Lets Aria hear the Aria key and type for you.")
                }
            } footer: {
                Text("Setup continues when Aria is turned on.")
            }
        }
    }

    private var paneName: String {
        if #available(macOS 27, *) {
            return "Device Control and Data Access"
        }
        return "Accessibility"
    }
}
