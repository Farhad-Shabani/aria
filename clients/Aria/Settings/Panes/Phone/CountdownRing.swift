import SwiftUI

struct CountdownRing: View {
    private static let lineWidth: CGFloat = 2

    let fraction: Double

    var body: some View {
        ZStack {
            Circle().stroke(.quaternary, lineWidth: Self.lineWidth)
            Circle()
                .trim(from: 0, to: fraction)
                .stroke(.secondary, style: StrokeStyle(lineWidth: Self.lineWidth, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: 12, height: 12)
        .animation(.linear(duration: 1), value: fraction)
    }
}
