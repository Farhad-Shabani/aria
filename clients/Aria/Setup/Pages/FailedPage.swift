import SwiftUI

struct FailedPage: View {
    let message: String

    var body: some View {
        ContentUnavailableView {
            Image(systemName: "exclamationmark.triangle")
                .font(.largeTitle)
                .accessibilityHidden(true)
        } description: {
            Text(markdown: message).textSelection(.enabled)
        }
    }
}
