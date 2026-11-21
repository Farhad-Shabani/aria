import SwiftUI

/// The rows are not buttons, because the flow decides which page comes next.
struct SetupRail: View {
    let progress: SetupProgress
    /// How far the install has come, while it runs.
    var installFraction: Double?

    var body: some View {
        List {
            Section {
                // The rows' marks show where setup stands, as Installer's sidebar does.
                SidebarHeader {
                    Text("Set Up Aria").font(.headline)
                }
            }
            Section {
                ForEach(SetupProgress.Stage.allCases, id: \.self) { stage in
                    StageRow(
                        stage: stage, status: progress.status(of: stage),
                        fraction: stage == .install ? installFraction : nil)
                }
            }
        }
    }
}
