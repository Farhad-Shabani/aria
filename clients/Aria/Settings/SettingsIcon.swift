import SwiftUI

struct SettingsIcon: View {
    let symbol: String
    var isWarning = false
    var side = SettingsMetrics.iconSide

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: side * SettingsMetrics.glyphRatio))
            .foregroundStyle(isWarning ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
            .frame(width: side, height: side)
            .accessibilityHidden(true)
    }
}

extension SettingsIcon {
    init(pane: SettingsPane, side: CGFloat = SettingsMetrics.iconSide) {
        self.init(symbol: pane.symbol, side: side)
    }
}
