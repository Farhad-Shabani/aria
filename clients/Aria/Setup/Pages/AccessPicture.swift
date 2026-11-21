import SwiftUI

/// The System Settings list Aria waits on, drawn small with Aria's switch
/// picked out, so the person knows what to look for there.
struct AccessPicture: View {
    private static let width: CGFloat = 340
    private static let cornerRadius: CGFloat = 8
    /// Between the toolbar and the list, and between the toolbar's parts.
    private static let spacing: CGFloat = 10
    private static let rowInset: CGFloat = 10
    private static let rowSpacing: CGFloat = 8
    /// The gray bar that stands for a neighbor's name.
    private static let nameBarHeight: CGFloat = 8
    private static let listFill = 0.4
    /// Neighbors fade with distance from Aria's row, as the keyboard drawing's keys do.
    private static let nearNeighbor: Neighbor = (nameWidth: 0.45, opacity: 0.4)
    private static let farNeighbor: Neighbor = (nameWidth: 0.3, opacity: 0.25)
    /// An app icon's corner, as a share of its side.
    private static let iconCornerRatio: CGFloat = 0.22

    private typealias Neighbor = (nameWidth: CGFloat, opacity: Double)

    let pane: String
    @ScaledMetric(relativeTo: .body) private var rowHeight: CGFloat = 32
    @ScaledMetric(relativeTo: .body) private var iconSide = SettingsMetrics.iconSide

    var body: some View {
        VStack(alignment: .leading, spacing: Self.spacing) {
            HStack(spacing: Self.spacing) {
                Image(systemName: "chevron.left")
                Image(systemName: "chevron.right")
                Text(pane).font(.headline).foregroundStyle(.primary).lineLimit(1)
            }
            .foregroundStyle(.tertiary)
            VStack(spacing: 0) {
                neighbor(Self.nearNeighbor)
                Divider()
                aria
                Divider()
                neighbor(Self.farNeighbor)
            }
            .background(.quaternary.opacity(Self.listFill), in: .rect(cornerRadius: Self.cornerRadius))
        }
        .frame(maxWidth: Self.width)
        .accessibilityElement()
        .accessibilityLabel("In \(pane), turn on Aria.")
    }

    private var aria: some View {
        row {
            AppIcon(side: iconSide)
            Text("Aria")
            Spacer()
            SwitchDrawing(isPickedOut: true)
        }
        .background(.tint.opacity(SetupMetrics.highlightOpacity))
    }

    private func neighbor(_ look: Neighbor) -> some View {
        row {
            RoundedRectangle(cornerRadius: iconSide * Self.iconCornerRatio, style: .continuous)
                .fill(.quaternary)
                .frame(width: iconSide, height: iconSide)
            GeometryReader { space in
                Capsule()
                    .fill(.quaternary)
                    .frame(width: space.size.width * look.nameWidth, height: Self.nameBarHeight)
                    .frame(maxHeight: .infinity)
            }
            SwitchDrawing(isPickedOut: false)
        }
        .opacity(look.opacity)
    }

    private func row(@ViewBuilder _ content: () -> some View) -> some View {
        HStack(spacing: Self.rowSpacing, content: content)
            .padding(.horizontal, Self.rowInset)
            .frame(height: rowHeight)
    }
}

/// An off switch, drawn: the picture's switches aren't controls.
private struct SwitchDrawing: View {
    private static let size = CGSize(width: 26, height: 15)
    private static let knobInset: CGFloat = 1.5
    private static let knob = Color.white
    private static let knobShadow = Color.black.opacity(0.2)

    let isPickedOut: Bool

    var body: some View {
        Capsule()
            .fill(.quaternary)
            .overlay {
                if isPickedOut { Capsule().strokeBorder(.tint, lineWidth: SetupMetrics.highlightStroke) }
            }
            .frame(width: Self.size.width, height: Self.size.height)
            .overlay(alignment: .leading) {
                Circle()
                    .fill(Self.knob)
                    .shadow(color: Self.knobShadow, radius: 0.5, y: 0.5)
                    .padding(Self.knobInset)
            }
    }
}
