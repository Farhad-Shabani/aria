import SwiftUI

/// The System Settings window Aria waits on, drawn small: Aria's row among
/// other apps, with the pointer on its switch, so the person knows what to
/// look for there.
struct AccessPicture: View {
    private static let width: CGFloat = 380
    private static let windowRadius: CGFloat = 12
    private static let boxRadius: CGFloat = 8
    private static let shadow = Color.black.opacity(0.12)
    private static let trafficLight: CGFloat = 10
    /// Around the window's content.
    private static let inset: CGFloat = 14
    /// Between the window's parts: toolbar, description, list, and buttons.
    private static let spacing: CGFloat = 10
    private static let toolbarSpacing: CGFloat = 14
    private static let chevronSpacing: CGFloat = 10
    private static let rowInset: CGFloat = 10
    private static let rowSpacing: CGFloat = 8
    /// A gray bar stands for text that doesn't matter here.
    private static let barHeight: CGFloat = 7
    /// An app icon's corner, as a share of its side.
    private static let iconCornerRatio: CGFloat = 0.22
    /// The list goes on past the drawing, fading as it goes.
    private static let before: [Neighbor] = [(nameWidth: 0.42, opacity: 0.55)]
    private static let after: [Neighbor] = [(nameWidth: 0.3, opacity: 0.4), (nameWidth: 0.5, opacity: 0.2)]

    private typealias Neighbor = (nameWidth: CGFloat, opacity: Double)

    let pane: String
    @ScaledMetric(relativeTo: .body) private var rowHeight: CGFloat = 34
    @ScaledMetric(relativeTo: .body) private var iconSide = SettingsMetrics.iconSide

    var body: some View {
        VStack(alignment: .leading, spacing: Self.spacing) {
            toolbar
            bar(width: 0.55)
            list
            HStack(spacing: Self.rowSpacing) {
                Image(systemName: "plus")
                Image(systemName: "minus")
            }
            .font(.caption)
            .foregroundStyle(.tertiary)
        }
        .font(.body)
        .padding(Self.inset)
        .frame(maxWidth: Self.width)
        .background(.background, in: .rect(cornerRadius: Self.windowRadius))
        .overlay { RoundedRectangle(cornerRadius: Self.windowRadius).strokeBorder(.separator) }
        .shadow(color: Self.shadow, radius: 8, y: 3)
        .accessibilityElement()
        .accessibilityLabel("In \(pane), turn on Aria.")
    }

    private var toolbar: some View {
        HStack(spacing: Self.toolbarSpacing) {
            HStack(spacing: Self.trafficLight * 0.8) {
                ForEach(0..<3, id: \.self) { _ in
                    Circle().fill(.quaternary).frame(width: Self.trafficLight, height: Self.trafficLight)
                }
            }
            HStack(spacing: Self.chevronSpacing) {
                Image(systemName: "chevron.left")
                Image(systemName: "chevron.right")
            }
            .foregroundStyle(.tertiary)
            Text(pane).font(.headline).lineLimit(1)
        }
    }

    private var list: some View {
        VStack(spacing: 0) {
            ForEach(Self.before.indices, id: \.self) { neighbor(Self.before[$0]) }
            Divider()
            aria
            ForEach(Self.after.indices, id: \.self) { index in
                Divider()
                neighbor(Self.after[index])
            }
        }
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: Self.boxRadius))
    }

    private var aria: some View {
        row {
            AppIcon(side: iconSide)
            Text("Aria")
            Spacer()
            OffSwitch()
                .overlay {
                    // The arrow's tip is its top-left corner, so it lands on the switch.
                    Image(systemName: "cursorarrow")
                        .font(.title3)
                        .symbolRenderingMode(.palette)
                        .foregroundStyle(.primary, .background)
                        .offset(x: iconSide * 0.4, y: iconSide * 0.5)
                }
        }
        .background(.tint.opacity(SetupMetrics.highlightOpacity))
    }

    private func neighbor(_ look: Neighbor) -> some View {
        row {
            RoundedRectangle(cornerRadius: iconSide * Self.iconCornerRatio, style: .continuous)
                .fill(.tertiary)
                .frame(width: iconSide, height: iconSide)
            bar(width: look.nameWidth)
            OffSwitch()
        }
        .opacity(look.opacity)
    }

    private func row(@ViewBuilder _ content: () -> some View) -> some View {
        HStack(spacing: Self.rowSpacing, content: content)
            .padding(.horizontal, Self.rowInset)
            .frame(height: rowHeight)
    }

    private func bar(width share: CGFloat) -> some View {
        GeometryReader { space in
            Capsule()
                .fill(.quaternary)
                .frame(width: space.size.width * share, height: Self.barHeight)
                .frame(maxHeight: .infinity)
        }
        .frame(height: Self.barHeight)
    }
}

/// The real switch, off, drawn but never clicked: the person turns on the one
/// in System Settings.
private struct OffSwitch: View {
    var body: some View {
        Toggle("Aria", isOn: .constant(false))
            .labelsHidden()
            .toggleStyle(.switch)
            .controlSize(.mini)
            .allowsHitTesting(false)
            .focusable(false)
    }
}
