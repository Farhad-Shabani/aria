import SwiftUI

struct ItemLink<Label: View>: View {
    let open: () -> Void
    @ViewBuilder let label: Label

    var body: some View {
        Button(action: open) {
            HStack {
                // Filled, so a row's own Spacer sets its detail beside the chevron.
                HStack { label }
                    .frame(maxWidth: .infinity, alignment: .leading)
                Image(systemName: "chevron.right")
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
