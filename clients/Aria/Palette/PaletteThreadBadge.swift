import SwiftUI

struct PaletteThreadBadge: View {
    let count: Int
    let isOpen: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 3) {
                Text("+\(count)").monospacedDigit()
                Image(systemName: isOpen ? "chevron.up" : "chevron.down")
                    .imageScale(.small)
            }
            .font(.caption.weight(.semibold))
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
