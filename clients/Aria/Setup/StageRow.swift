import SwiftUI

struct StageRow: View {
    let stage: SetupProgress.Stage
    let status: SetupProgress.Status

    var body: some View {
        Label {
            Text(stage.title)
                .fontWeight(status == .current ? .semibold : .regular)
                .foregroundStyle(status == .upcoming || status == .skipped ? .secondary : .primary)
        } icon: {
            StatusMark(status: status)
        }
        .accessibilityValue(spokenStatus)
    }

    private var spokenStatus: String {
        switch status {
        case .done: "Done"
        case .current: "Current step"
        case .upcoming: "Not started"
        case .skipped: "Skipped"
        case .failed: "Failed"
        }
    }
}
