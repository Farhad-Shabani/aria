import SwiftUI

struct PaletteCommandRow: View {
    let command: PaletteCommand
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        PaletteRow(symbol: command.symbol, isSelected: isSelected, action: action) {
            Text(command.title)
                .paletteRowText()
            if let chord = command.chord {
                ChordLabel(chord: chord)
                    .foregroundStyle(.secondary)
                    .fixedSize()
            }
            if let key = command.key {
                PaletteKeyCap(key.label)
            }
            if let number = command.number {
                PaletteKeyCap(String(number))
            }
        }
    }
}
