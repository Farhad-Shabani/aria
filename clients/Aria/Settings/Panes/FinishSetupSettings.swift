import AppKit
import ServiceManagement
import SwiftUI

struct FinishSetupSettings: View {
    let items: [SetupItem]
    let setup: SetupStatus
    let download: ModelDownload
    let wizard: SetupState
    let health: SettingsHealth
    let poller: StatusPoller
    let open: (SettingsPane) -> Void

    var body: some View {
        Form {
            Section {
                ForEach(items, id: \.self) { item in
                    switch item {
                    case .backgroundService:
                        row(item) { Button("Open System Settings") { SMAppService.openSystemSettingsLoginItems() } }
                    case .settingsFileStopsStart, .settingsFileIgnored:
                        row(item) { Button("Open Settings File") { Task { await health.openSettingsFile() } } }
                    case .server:
                        row(item) { Button("Show Recognition") { open(.recognition) } }
                    case .speechModel(let id):
                        SpeechModelRow(id: id, download: download, wizard: wizard) {
                            await setup.refresh()
                            await poller.ensureRunning()
                        }
                    case .ariaKey:
                        row(item) { Button("Open System Settings") { SetupClient.openAccessibility() } }
                    case .ariaKeyRemapped:
                        row(item) { Button("Open Keyboard Settings") { SetupClient.openKeyboard() } }
                    }
                }
            }
        }
    }

    private func row(_ item: SetupItem, @ViewBuilder action: () -> some View) -> some View {
        LabeledContent {
            action()
        } label: {
            Text(item.title)
            Text(item.detail)
        }
    }
}

/// The speech model downloads in place, so its row shows the download.
private struct SpeechModelRow: View {
    let id: String
    @ObservedObject var download: ModelDownload
    @ObservedObject var wizard: SetupState
    let downloaded: @MainActor () async -> Void

    private var item: SetupItem { .speechModel(id: id) }

    var body: some View {
        LabeledContent {
            button
        } label: {
            Text(item.title)
            status
        }
        .onChange(of: download.state) { _, state in
            guard state == .failed else { return }
            NSAccessibility.post(
                element: NSApp as Any,
                notification: .announcementRequested,
                userInfo: [.announcement: ModelDownload.failure]
            )
        }
    }

    @ViewBuilder
    private var status: some View {
        switch download.state {
        case .idle: Text(item.detail)
        case .downloading(let fraction):
            ProgressView(value: fraction)
                .accessibilityLabel("Downloading speech model")
            Text("Downloading…")
        case .finishing:
            ProgressView()
                .accessibilityLabel("Finishing setup")
            Text("Finishing…")
        case .failed: Text(ModelDownload.failure)
        }
    }

    @ViewBuilder
    private var button: some View {
        switch download.state {
        case .downloading, .finishing:
            Button("Cancel") { download.cancel() }
                .disabled(download.state == .finishing)
        case .idle, .failed:
            // Two setup runs at once make the second one fail.
            Button("Download") { download.start(id, then: downloaded) }
                .disabled(wizard.step == .applying)
        }
    }
}
