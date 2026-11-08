import SwiftUI

struct SpeechModelPicker: View {
    private static let title = "Speech model"
    /// Tags the server's row, which names no local engine.
    private static let serverTag = "server"

    @ObservedObject var setup: SetupStatus
    @ObservedObject var engineChange: TranscriptionEngineChange
    @EnvironmentObject private var runner: SetupRunner

    private var models: SpeechModels { setup.speechModels }

    private var target: SpeechModels.Model? {
        engineChange.engine.flatMap(models.model)
    }

    private var usesServer: Bool { setup.transcriptionRunsOn == .server }

    /// The menu shows a new choice at once, so a change reads as done.
    private var shown: String {
        if usesServer { return Self.serverTag }
        if engineChange.isRunning, let target { return target.id }
        return models.current ?? ""
    }

    var body: some View {
        LabeledContent {
            HStack {
                accessory
                picker
            }
        } label: {
            Text(Self.title)
            status
        }
        .onChange(of: engineChange.state) { _, state in
            guard case .failed(let message) = state, target != nil else { return }
            NSAccessibility.announce(message)
        }
        .onChange(of: models.current) { previous, current in
            guard previous != nil, let model = current.flatMap(models.model) else { return }
            NSAccessibility.announce("Using \(model.label)")
        }
    }

    /// The rest of a switch takes only a moment, so it gets a spinner and no Cancel.
    @ViewBuilder
    private var accessory: some View {
        if let target, engineChange.isRunning {
            switch engineChange.state {
            case .downloading:
                Button("Cancel") { engineChange.cancel() }
            case .finishing:
                ProgressView()
                    .controlSize(.small)
                    .accessibilityLabel("Switching to \(target.label)")
            case .idle, .failed:
                EmptyView()
            }
        }
    }

    @ViewBuilder
    private var status: some View {
        if usesServer {
            Text("Your server transcribes. Change this in Recognition.")
        } else if let target, case .downloading(let transfer) = engineChange.state {
            if let transfer {
                Text(
                    "Downloading \(target.label)… \(SetupState.size(transfer.received)) of \(SetupState.size(transfer.total))"
                )
                .monospacedDigit()
                ProgressView(value: transfer.fraction)
                    .accessibilityLabel("Downloading \(target.label)")
            } else {
                Text("Downloading \(target.label)…")
            }
        } else if case .failed(let message) = engineChange.state, target != nil {
            Text(message).foregroundStyle(.failure)
        }
    }

    private var picker: some View {
        Picker(Self.title, selection: Binding(get: { shown }, set: { choose($0) })) {
            if usesServer {
                Text("Your server").tag(Self.serverTag)
            }
            ForEach(models.onMac) { model in
                Text(model.label).tag(model.id)
            }
            if !models.toDownload.isEmpty {
                Section("Download") {
                    ForEach(models.toDownload) { model in
                        let size = model.size ?? ""
                        Text("\(model.label)   \(Text(size).foregroundStyle(.secondary))")
                            .accessibilityLabel("\(model.label), download size: \(size)")
                            .tag(model.id)
                    }
                }
            }
        }
        .labelsHidden()
        .fixedSize()
        .disabled(usesServer || runner.isRunning)
    }

    private func choose(_ id: String) {
        guard id != models.current, models.model(id) != nil else { return }
        engineChange.start(id) { await setup.refresh() }
    }
}
