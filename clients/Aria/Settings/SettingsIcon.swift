import SwiftUI

struct SettingsIcon: View {
    let pane: SettingsPane
    var side = SettingsMetrics.iconSide

    var body: some View {
        Image(systemName: pane.symbol)
            .font(.system(size: side * SettingsMetrics.glyphRatio, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: side, height: side)
            .background(
                pane.tint.gradient,
                in: RoundedRectangle(cornerRadius: side * SettingsMetrics.cornerRatio, style: .continuous)
            )
            .accessibilityHidden(true)
    }
}
