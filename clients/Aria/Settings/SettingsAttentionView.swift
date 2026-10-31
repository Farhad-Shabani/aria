import SwiftUI
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SettingsAttention")

/// The settings file's problems and notices, in the menu and in Settings.
struct SettingsAttentionView: View {
    @ObservedObject var health: SettingsHealth
    @ObservedObject var poller: StatusPoller
    @ObservedObject var settings: DaemonSettings

    private var items: [SettingsAttention] {
        SettingsAttention.all(
            status: health.status,
            startFailure: poller.failure,
            isReadOnly: settings.isReadOnlyFailure,
            hasUnsavedChange: settings.hasUnsavedChange && !settings.isReadOnlyFailure
        )
    }

    var body: some View {
        ForEach(items, id: \.self) { item in
            VStack(alignment: .leading, spacing: 4) {
                Text(item.message)
                    .font(.caption)
                    .foregroundStyle(item.isProblem ? AnyShapeStyle(.failure) : AnyShapeStyle(.secondary))
                    .frame(maxWidth: .infinity, alignment: .leading)
                if item.opensFile {
                    Button("Open Settings File") {
                        Task { await health.openSettingsFile() }
                    }
                }
                if item == .restartNeeded {
                    HStack {
                        Button("Restart to Apply") {
                            Task { await restart() }
                        }
                        .disabled(poller.state == .working)
                        if poller.state == .working {
                            ProgressView().controlSize(.small)
                        }
                    }
                }
            }
        }
        if let openFailure = health.openFailure {
            NoticeText(notice: openFailure).font(.caption)
        }
    }

    private func restart() async {
        do {
            try await poller.restart()
        } catch {
            log.error("Restart to apply failed: \(String(describing: error), privacy: .private)")
        }
    }
}
