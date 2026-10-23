import SwiftUI

struct StatusMark: View {
    let status: SetupProgress.Status

    var body: some View {
        Image(systemName: symbol)
            .foregroundStyle(tint)
            .accessibilityHidden(true)
    }

    private var symbol: String {
        switch status {
        case .done: "checkmark.circle.fill"
        case .current: "circle.inset.filled"
        case .upcoming: "circle"
        case .skipped: "minus.circle"
        case .failed: "xmark.circle.fill"
        }
    }

    private var tint: AnyShapeStyle {
        switch status {
        case .done, .current: AnyShapeStyle(.tint)
        case .upcoming: AnyShapeStyle(.secondary)
        case .skipped: AnyShapeStyle(.tertiary)
        case .failed: AnyShapeStyle(.red)
        }
    }
}
