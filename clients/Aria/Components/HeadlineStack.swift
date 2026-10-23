import SwiftUI

struct HeadlineStack<Subtitle: View>: View {
    let title: String
    @ViewBuilder let subtitle: Subtitle

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.headline)
            subtitle
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
    }
}
