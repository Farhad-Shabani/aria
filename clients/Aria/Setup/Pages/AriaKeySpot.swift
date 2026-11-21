import SwiftUI

/// The Aria key in its place on the keyboard, among neighbors that fade with
/// distance, so the person finds it without reading a key name.
struct AriaKeySpot: View {
    let key: AriaKey
    /// One key width. The drawing is laid out in key widths, so it grows with the text size.
    @ScaledMetric(relativeTo: .body) private var unit: CGFloat = 44

    var body: some View {
        let caps = Cap.layout(for: key)
        let bounds = Cap.bounds(of: caps)
        ZStack {
            ForEach(caps) { cap in
                KeyCap(cap: cap, unit: unit)
                    .frame(width: unit * cap.width, height: unit * cap.height)
                    .opacity(cap.opacity)
                    .offset(
                        x: unit * (cap.x + cap.width / 2 - bounds.midX),
                        y: unit * (cap.y + cap.height / 2 - bounds.midY))
            }
        }
        .frame(width: unit * bounds.width, height: unit * bounds.height)
        .frame(maxWidth: .infinity)
        .accessibilityElement()
        .accessibilityLabel("Aria key, \(key.title)")
    }
}

private struct Cap: Identifiable {
    enum Face {
        /// A modifier: its symbol above its name.
        case modifier(symbol: String, name: String)
        /// A character key: its shifted mark over its plain one.
        case marks(String, String)
        case arrow(symbol: String)
    }

    let face: Face
    /// Key widths from the Aria key's top-left corner.
    let x: CGFloat
    let y: CGFloat
    var width: CGFloat = 1
    var height: CGFloat = 1
    var opacity: Double = 1
    var isAriaKey = false

    var id: String { "\(x),\(y)" }

    /// The box around every key, in key widths.
    static func bounds(of caps: [Cap]) -> CGRect {
        caps.map { CGRect(x: $0.x, y: $0.y, width: $0.width, height: $0.height) }
            .reduce(CGRect.null) { $0.union($1) }
    }

    static func layout(for key: AriaKey) -> [Cap] {
        switch key {
        case .rightOption:
            [
                Cap(face: .modifier(symbol: "command", name: "command"), x: -1.37, y: 0, width: 1.25, opacity: 0.5),
                Cap(face: .arrow(symbol: "arrowtriangle.left.fill"), x: 1.12, y: 0.52, height: 0.48, opacity: 0.5),
                Cap(face: .arrow(symbol: "arrowtriangle.up.fill"), x: 2.24, y: 0, height: 0.46, opacity: 0.25),
                Cap(face: .arrow(symbol: "arrowtriangle.down.fill"), x: 2.24, y: 0.54, height: 0.46, opacity: 0.25),
                Cap(face: .marks("<", ","), x: -1.25, y: -1.12, opacity: 0.35),
                Cap(face: .marks(">", "."), x: -0.13, y: -1.12, opacity: 0.5),
                Cap(face: .marks("?", "/"), x: 0.99, y: -1.12, opacity: 0.35),
                Cap(face: .modifier(symbol: "option", name: "option"), x: 0, y: 0, isAriaKey: true),
            ]
        case .fn:
            [
                Cap(face: .modifier(symbol: "control", name: "control"), x: 1.12, y: 0, opacity: 0.5),
                Cap(face: .modifier(symbol: "option", name: "option"), x: 2.24, y: 0, opacity: 0.25),
                Cap(face: .modifier(symbol: "shift", name: "shift"), x: 0, y: -1.12, width: 2.25, opacity: 0.4),
                Cap(face: .marks("Z", ""), x: 2.37, y: -1.12, opacity: 0.2),
                Cap(face: .modifier(symbol: "globe", name: "fn"), x: 0, y: 0, isAriaKey: true),
            ]
        }
    }
}

private struct KeyCap: View {
    let cap: Cap
    let unit: CGFloat

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: unit * 0.15, style: .continuous)
        face
            .foregroundStyle(cap.isAriaKey ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
            .padding(.vertical, unit * 0.12)
            .padding(.horizontal, unit * 0.14)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background {
                if cap.isAriaKey {
                    shape.fill(.tint.opacity(0.15))
                } else {
                    shape.fill(.quaternary)
                }
            }
            .overlay {
                if cap.isAriaKey { shape.strokeBorder(.tint, lineWidth: 1.5) }
            }
    }

    @ViewBuilder private var face: some View {
        switch cap.face {
        case .modifier(let symbol, let name):
            VStack(alignment: .leading, spacing: 0) {
                Image(systemName: symbol).font(.callout)
                Spacer(minLength: 0)
                Text(name).font(.caption2).lineLimit(1).minimumScaleFactor(0.6)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        case .marks(let shifted, let plain):
            VStack(spacing: 0) {
                Text(shifted)
                if !plain.isEmpty { Text(plain) }
            }
            .font(.callout)
        case .arrow(let symbol):
            Image(systemName: symbol).font(.caption2).imageScale(.small)
        }
    }
}
