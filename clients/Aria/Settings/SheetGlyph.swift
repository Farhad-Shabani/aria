import SwiftUI

struct SheetGlyph: View {
    let symbol: String
    let tint: Color

    var body: some View {
        Image(systemName: symbol)
            .font(.title)
            .foregroundStyle(.white)
            .frame(width: SettingsMetrics.sheetGlyphSide, height: SettingsMetrics.sheetGlyphSide)
            .background(Circle().fill(tint))
            .accessibilityHidden(true)
    }
}
