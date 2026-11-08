import SwiftUI

struct PalettePageProgress: View {
    private let title: String

    var body: some View {
        VStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text(title).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension PalettePageProgress {
    init(_ title: String) {
        self.title = title
    }
}
