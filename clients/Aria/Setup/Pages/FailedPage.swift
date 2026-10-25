import SwiftUI

struct FailedPage: View {
    let message: String

    var body: some View {
        ContentUnavailableView {
            Image(systemName: "exclamationmark.triangle").font(.largeTitle)
        } description: {
            Text(message).textSelection(.enabled)
        }
    }
}
