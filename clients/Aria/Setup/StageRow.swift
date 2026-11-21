import SwiftUI

struct StageRow: View {
    let stage: SetupProgress.Stage
    let status: SetupProgress.Status
    var fraction: Double?

    var body: some View {
        Label {
            Text(stage.title)
                .fontWeight(status == .current ? .semibold : .regular)
                .foregroundStyle(status == .upcoming || status == .skipped ? .secondary : .primary)
        } icon: {
            StatusMark(status: status, fraction: fraction)
        }
        .accessibilityValue(spokenStatus)
    }

    private var spokenStatus: String {
        switch status {
        case .done: "Done"
        case .current:
            fraction.map { "Current step, \($0.formatted(.percent.precision(.fractionLength(0))))" } ?? "Current step"
        case .upcoming: "Not started"
        case .skipped: "Skipped"
        case .failed: "Didn’t finish"
        }
    }
}
