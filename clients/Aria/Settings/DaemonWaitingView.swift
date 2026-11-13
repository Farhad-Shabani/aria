import SwiftUI

struct DaemonWaitingView: View {
    let isLoading: Bool
    var description: LocalizedStringKey = "These settings load once Aria is running."

    var body: some View {
        ContentUnavailableView {
            Label("Waiting for Aria", systemImage: "hourglass")
        } description: {
            Text(description)
        } actions: {
            if isLoading {
                ProgressView().controlSize(.small)
            }
        }
    }
}
