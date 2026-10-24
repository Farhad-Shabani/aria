import SwiftUI

struct AboutSettings: View {
    @ObservedObject var updates: UpdateController
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
                    Button("Check for Updates…") { updates.checkForUpdates() }
                        .disabled(!updates.canCheck)
                        .padding(.top, 8)
                }
                .frame(maxWidth: .infinity)
            }
            Section {
                Toggle(
                    "Automatically check for updates",
                    isOn: Binding(get: { updates.checksAutomatically }, set: { updates.setChecksAutomatically($0) })
                )
                Toggle(
                    "Automatically download and install",
                    isOn: Binding(get: { updates.installsAutomatically }, set: { updates.setInstallsAutomatically($0) })
                )
                .disabled(!updates.checksAutomatically)
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
