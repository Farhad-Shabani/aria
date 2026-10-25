import SwiftUI

struct InstallingPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                ForEach(state.installRows, id: \.step) { row in
                    LabeledContent {
                        mark(row.progress)
                    } label: {
                        Text(row.title).foregroundStyle(row.progress == .pending ? .secondary : .primary)
                    }
                    .accessibilityValue(spoken(row.progress))
                    if row.step == .models {
                        ForEach(state.downloadProgress.keys.sorted(), id: \.self) { model in
                            if let progress = state.downloadProgress[model] { bar(model, progress) }
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder private func mark(_ progress: SetupState.InstallRow.Progress) -> some View {
        switch progress {
        case .pending:
            StatusMark(status: .upcoming)
        case .running:
            ProgressView().controlSize(.small)
        case .done:
            StatusMark(status: .done)
        }
    }

    /// The mark is hidden from VoiceOver, so the row says its progress.
    private func spoken(_ progress: SetupState.InstallRow.Progress) -> String {
        switch progress {
        case .pending: "Not started"
        case .running: "In progress"
        case .done: "Done"
        }
    }

    private func bar(_ model: String, _ progress: Aria_Setup_V1_DownloadProgress) -> some View {
        let received = min(progress.received, progress.total)
        let title = state.inspection.models.first { $0.name == model }?.title ?? model
        return ProgressView(value: Double(received), total: Double(max(progress.total, 1))) {
            Text("\(title) · \(SetupState.size(received)) of \(SetupState.size(progress.total))")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
    }
}
