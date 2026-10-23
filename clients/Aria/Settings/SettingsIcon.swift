import SwiftUI

struct SettingsIcon: View {
    let pane: SettingsPane

    var body: some View {
        let side = SettingsMetrics.iconSide
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
