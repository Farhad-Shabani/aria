import SwiftUI

struct AboutSettings: View {
    private static let legal = "https://github.com/Farhad-Shabani/aria/blob/main/legal/"
    private let licenses = Bundle.main.resourceURL?.appending(path: "Licenses")

    var body: some View {
        Form {
            Section {
                VStack(spacing: 4) {
                    AppIcon(side: SettingsMetrics.heroIconSide)
                    Text(Bundle.main.displayName).font(.title2.weight(.semibold))
                    if let version = Bundle.main.shortVersion {
                        Text("Version \(version)").foregroundStyle(.secondary)
                    }
                    if let copyright = Bundle.main.copyright {
                        Text(copyright).font(.caption).foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
            }
            Section {
                if let url = URL(string: Self.legal + "privacy-policy.md") {
                    Link("Privacy Policy", destination: url)
                }
                if let url = URL(string: Self.legal + "terms.md") {
                    Link("Terms of Service", destination: url)
                }
                if let licenses, FileManager.default.fileExists(atPath: licenses.path) {
                    Button("Licenses") { NSWorkspace.shared.open(licenses) }
                        .buttonStyle(.link)
                }
            }
        }
    }
}
