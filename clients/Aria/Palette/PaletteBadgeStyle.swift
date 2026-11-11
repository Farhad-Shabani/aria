import SwiftUI

/// The capsule a row's badges sit in, before its time.
struct PaletteBadgeStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(.quinary, in: Capsule())
            .contentShape(Capsule())
    }
}

extension ButtonStyle where Self == PaletteBadgeStyle {
    static var paletteBadge: PaletteBadgeStyle { PaletteBadgeStyle() }
}
