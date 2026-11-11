import SwiftUI

struct PaletteThreadBadge: View {
    let count: Int
    let isOpen: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            // VoiceOver reads the count. On screen, the chevron shows there are results.
            Image(systemName: isOpen ? "chevron.down" : "chevron.forward")
                .imageScale(.small)
                .font(.caption.weight(.bold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 7)
                .padding(.vertical, 2)
                .background(.quinary, in: Capsule())
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .help(isOpen ? "Hide Results (←)" : "Show Results (→)")
        .accessibilityLabel(isOpen ? "Hide Results" : "Show Results")
        .accessibilityValue(PromptResult.countLabel(count))
    }
}
