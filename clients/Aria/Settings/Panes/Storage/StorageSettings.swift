import SwiftUI

struct StorageSettings: View {
    @ObservedObject var setup: SetupStatus
    @EnvironmentObject private var runner: SetupRunner
    @StateObject private var removal = ModelRemoval()
    @State private var confirming: DownloadedModels.Model?

    private var models: DownloadedModels { setup.downloadedModels }

    var body: some View {
        Form {
            ItemPage(title: "Storage", subtitle: "\(SetupState.size(models.total)) on your Mac") {
                SettingsIcon(symbol: "internaldrive", side: SettingsMetrics.headerIconSide)
            } content: {
                Section {
                    ForEach(models.models) { model in
                        row(model)
                    }
                } header: {
                    Text("Models")
                } footer: {
                    if removal.failure != nil {
                        NoticeText(notice: removal.failure)
                    } else {
                        Text("Choose a model again to download it.")
                    }
                }
            }
        }
        .confirmationDialog(
            "Delete \(confirming?.name ?? "")?",
            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
            presenting: confirming
        ) { model in
            Button("Delete", role: .destructive) {
                Task { await removal.remove(model, using: runner) { await setup.refresh() } }
            }
        } message: { model in
            if !model.isPartial {
                Text("This frees \(SetupState.size(model.bytes)).")
            }
        }
        .task { await setup.refresh() }
    }

    private func row(_ model: DownloadedModels.Model) -> some View {
        LabeledContent {
            if removal.removing == model.id {
                ProgressView()
                    .controlSize(.small)
                    .accessibilityLabel("Deleting \(model.name)")
            } else if model.usedBy.isEmpty {
                Button(role: .destructive) {
                    confirming = model
                } label: {
                    Image(systemName: "trash")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel("Delete \(model.name)")
                .disabled(removal.removing != nil || runner.isRunning)
            }
        } label: {
            Text(model.name)
            Text(model.detail)
        }
    }
}
