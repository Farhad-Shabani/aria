import SwiftUI

struct DaemonWaitingView: View {
    var body: some View {
        ContentUnavailableView(
            "Waiting for Aria",
            systemImage: "hourglass",
            description: Text("These settings load once Aria is running.")
        )
    }
}
