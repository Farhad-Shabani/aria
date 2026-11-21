import SwiftUI

struct SettingsIcon: View {
    let symbol: String
    var isWarning = false
    var side = IconMetrics.rowSide

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: side * SettingsMetrics.glyphRatio))
            .foregroundStyle(isWarning ? AnyShapeStyle(.attention) : AnyShapeStyle(.secondary))
            .frame(width: side, height: side)
            .accessibilityHidden(true)
    }
}

extension SettingsIcon {
    init(pane: SettingsPane, side: CGFloat = IconMetrics.rowSide) {
        self.init(symbol: pane.symbol, side: side)
    }
}
