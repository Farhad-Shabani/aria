import SwiftUI

struct PaletteReplyingBadge: View {
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            ProgressView()
                .controlSize(.mini)
        }
        .buttonStyle(.paletteBadge)
        .help("Show Chat")
        .accessibilityLabel("Show Chat")
        .accessibilityValue("Replying")
    }
}
