import SwiftUI

struct PaletteResultRow: View {
    let result: PromptResult
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        PaletteRow(
            symbol: "text.bubble", isSelected: isSelected, indent: PaletteRowMetrics.symbolWidth, action: action
        ) {
            HStack(spacing: 8) {
                Text(result.promptName).fixedSize()
                Text(firstLine).foregroundStyle(.secondary)
            }
            .paletteRowText()
            Text(HistoryDay.time(of: result.created))
                .paletteRowDetail()
        }
    }

    private var firstLine: String {
        result.text.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
    }
}
