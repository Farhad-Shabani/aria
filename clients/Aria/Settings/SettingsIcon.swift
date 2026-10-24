import SwiftUI

struct SettingsIcon: View {
    let symbol: String
    let tint: Color
    var side = SettingsMetrics.iconSide

    init(symbol: String, tint: Color, side: CGFloat = SettingsMetrics.iconSide) {
        self.symbol = symbol
        self.tint = tint
        self.side = side
    }

    init(pane: SettingsPane, side: CGFloat = SettingsMetrics.iconSide) {
        self.init(symbol: pane.symbol, tint: pane.tint, side: side)
    }

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: side * SettingsMetrics.glyphRatio, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: side, height: side)
            .background(
                tint.gradient,
                in: RoundedRectangle(cornerRadius: side * SettingsMetrics.cornerRatio, style: .continuous)
            )
            .accessibilityHidden(true)
    }
}
