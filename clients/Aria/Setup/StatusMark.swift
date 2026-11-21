import SwiftUI

struct StatusMark: View {
    /// A ring's empty part keeps the tint, so an empty ring still reads as current.
    private static let ringTrack = 0.3

    let status: SetupProgress.Status
    /// How far a long step has come. The current mark becomes a ring that fills.
    var fraction: Double?
    @ScaledMetric(relativeTo: .body) private var ringWidth: CGFloat = 2

    var body: some View {
        Group {
            if status == .current, let fraction {
                ring(fraction)
            } else {
                Image(systemName: symbol).foregroundStyle(tint)
            }
        }
        .accessibilityHidden(true)
    }

    /// Drawn over the empty circle symbol, so it keeps the other marks' size.
    private func ring(_ fraction: Double) -> some View {
        Image(systemName: "circle")
            .foregroundStyle(.tint.opacity(Self.ringTrack))
            .overlay {
                Circle()
                    .trim(from: 0, to: fraction)
                    .stroke(.tint, style: StrokeStyle(lineWidth: ringWidth, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                    .padding(ringWidth)
                    .animation(.smooth, value: fraction)
            }
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
        case .failed: AnyShapeStyle(.failure)
        }
    }
}
