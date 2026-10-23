import SwiftUI

extension Text {
    /// Explanatory text under a checkbox, indented past its box.
    func setupFootnote() -> some View {
        font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.leading, 20)
    }
}
