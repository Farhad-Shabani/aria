import SwiftUI

struct DaemonWaitingView: View {
    let isLoading: Bool
    let description: LocalizedStringKey

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
