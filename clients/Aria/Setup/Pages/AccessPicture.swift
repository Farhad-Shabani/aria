import SwiftUI

/// The System Settings list Aria waits on, drawn small with Aria's switch
/// picked out, so the person knows what to look for there.
struct AccessPicture: View {
    private static let cornerRadius: CGFloat = 8

    let pane: String
    @ScaledMetric(relativeTo: .body) private var rowHeight: CGFloat = 32
    @ScaledMetric(relativeTo: .body) private var iconSide: CGFloat = 20

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Image(systemName: "chevron.left")
                Image(systemName: "chevron.right")
                Text(pane).font(.headline).foregroundStyle(.primary).lineLimit(1)
            }
            .foregroundStyle(.tertiary)
            VStack(spacing: 0) {
                neighbor(nameWidth: 0.45).opacity(0.4)
                Divider()
                aria
                Divider()
                neighbor(nameWidth: 0.3).opacity(0.25)
            }
            .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: Self.cornerRadius))
        }
        .frame(maxWidth: 340)
        .frame(maxWidth: .infinity)
        .accessibilityElement()
        .accessibilityLabel("In \(pane), turn on Aria.")
    }

    private var aria: some View {
        HStack(spacing: 8) {
            AppIcon(side: iconSide)
            Text("Aria")
            Spacer()
            SwitchDrawing(isPickedOut: true)
        }
        .padding(.horizontal, 10)
        .frame(height: rowHeight)
        .background(.tint.opacity(0.12))
    }

    private func neighbor(nameWidth: CGFloat) -> some View {
        HStack(spacing: 8) {
            RoundedRectangle(cornerRadius: iconSide * 0.22, style: .continuous)
                .fill(.quaternary)
                .frame(width: iconSide, height: iconSide)
            GeometryReader { space in
                Capsule()
                    .fill(.quaternary)
                    .frame(width: space.size.width * nameWidth, height: 8)
                    .frame(maxHeight: .infinity)
            }
            SwitchDrawing(isPickedOut: false)
        }
        .padding(.horizontal, 10)
        .frame(height: rowHeight)
    }
}

/// An off switch, drawn: the picture's switches aren't controls.
private struct SwitchDrawing: View {
    private static let knob = Color.white
    private static let knobShadow = Color.black.opacity(0.2)

    let isPickedOut: Bool

    var body: some View {
        Capsule()
            .fill(.quaternary)
            .overlay { if isPickedOut { Capsule().strokeBorder(.tint, lineWidth: 1.5) } }
            .frame(width: 26, height: 15)
            .overlay(alignment: .leading) {
                Circle()
                    .fill(Self.knob)
                    .shadow(color: Self.knobShadow, radius: 0.5, y: 0.5)
                    .padding(1.5)
            }
    }
}
