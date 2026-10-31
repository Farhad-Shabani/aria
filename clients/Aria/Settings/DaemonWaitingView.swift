import SwiftUI

struct DaemonWaitingView: View {
    let isLoading: Bool

    var body: some View {
        ContentUnavailableView {
            Label("Waiting for Aria", systemImage: "hourglass")
        } description: {
            Text("These settings load once Aria is running.")
        } actions: {
            if isLoading {
                ProgressView().controlSize(.small)
            }
        }
    }
}
