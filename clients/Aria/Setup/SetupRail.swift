import SwiftUI

/// The rows are not buttons, because the flow decides which page comes next.
struct SetupRail: View {
    let progress: SetupProgress

    var body: some View {
        List {
            Section {
                SidebarHeader {
                    HeadlineStack(title: "Set Up Aria") {
                        ProgressView(value: progress.fraction)
                            .controlSize(.small)
                            .accessibilityLabel("Setup progress")
                    }
                }
            }
            Section {
                ForEach(SetupProgress.Stage.allCases, id: \.self) { stage in
                    StageRow(stage: stage, status: progress.status(of: stage))
                }
            }
        }
    }
}
