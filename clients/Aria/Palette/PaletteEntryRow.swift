import SwiftUI

struct PaletteEntryRow: View {
    let entry: HistoryItem
    let isSelected: Bool
    /// Set while the title is being edited in place.
    var titleField: PaletteTitleField?
    var badge: PaletteThreadBadge?
    let action: () -> Void

    var body: some View {
        PaletteRow(symbol: entry.kind.symbol, isSelected: isSelected, action: titleField == nil ? action : nil) {
            Group {
                if let titleField {
                    titleField
                } else {
                    Text(entry.name)
                }
            }
            .paletteRowText()
        } accessory: {
            // The count comes before the place, so every row's time lines up on the right. The
            // place sits outside the row's button, so its tap gesture opens the entry.
            badge
            Text(place)
                .paletteRowDetail()
                .contentShape(Rectangle())
                .onTapGesture { if titleField == nil { action() } }
        }
    }

    private var place: String {
        "\(entry.origin) · \(HistoryDay.time(of: entry.created))"
    }
}
