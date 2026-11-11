import SwiftUI

/// Laid out like a row, so the field lines up with the text above.
struct PaletteFollowUpRow: View {
    @ObservedObject var controller: PaletteController
    let isEnabled: Bool
    let placeholder: String

    var body: some View {
        HStack(spacing: PaletteRowMetrics.columnSpacing) {
            Image(systemName: "arrow.turn.down.right")
                .foregroundStyle(.secondary)
                .frame(width: PaletteRowMetrics.symbolWidth)
                .accessibilityHidden(true)
            PaletteFollowUpField(controller: controller, isEnabled: isEnabled, placeholder: placeholder)
        }
        .padding(.horizontal, PaletteBarMetrics.leading)
        .frame(minHeight: PaletteRowMetrics.height)
    }
}

/// A follow-up on the text a page shows, which starts a chat quoting it.
struct PaletteTextFollowUp: View {
    @ObservedObject var controller: PaletteController

    var body: some View {
        if let fits = controller.quoteFits {
            PaletteFollowUpRow(
                // Off while a name is edited, so the name field keeps focus.
                controller: controller, isEnabled: fits && !controller.model.isRenaming,
                placeholder: fits ? "Ask a follow-up" : "Too long to follow up on")
        }
    }
}
