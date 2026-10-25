import SwiftUI

struct WelcomePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                Label("Pick speech and chat engines", systemImage: "waveform")
                Label("Download the models they need", systemImage: "arrow.down.circle")
                Label("Run Aria in the background", systemImage: "gearshape")
            } footer: {
                if let missing = state.missingSummary {
                    Text(missing)
                }
                NoticeText(notice: state.blockedBy)
            }
            if state.step == .welcome { expressDownloads }
        }
    }

    private var expressDownloads: some View {
        let plan = SetupState.expressPlan(state.inspection, state.selections)
        return Section {
            ForEach(plan, id: \.name) { model in
                LabeledContent(model.title) {
                    Text(SetupState.downloadSize(of: [model])).monospacedDigit()
                }
            }
        } header: {
            if !plan.isEmpty {
                Text("Downloads")
            }
        } footer: {
            DownloadFooter(total: SetupState.total(plan), notes: state.selfFetchingNotes)
        }
    }
}
