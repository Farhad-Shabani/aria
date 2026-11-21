import SwiftUI

/// A small spinner beside what the person waits for, for a wait inside a
/// row, a footer, or a bar of buttons.
struct ProgressLabel: View {
    private static let spacing: CGFloat = 6

    private let title: Text

    init(_ title: LocalizedStringKey) {
        self.title = Text(title)
    }

    var body: some View {
        HStack(spacing: Self.spacing) {
            ProgressView().controlSize(.small)
            title.foregroundStyle(.secondary)
        }
    }
}
