import SwiftUI

/// The Aria key in its place on the keyboard, among neighbors that fade with
/// distance, so the person finds it without reading a key name.
struct AriaKeySpot: View {
    let key: AriaKey
    /// The key held with the Aria key, lit beside it.
    var combo: ShortcutCombo?
    /// One key width. The drawing is laid out in key widths, so it grows with the text size.
    @ScaledMetric(relativeTo: .body) private var unit: CGFloat = 44
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var press = Press.up

    var body: some View {
        let caps = Cap.layout(for: key, combo: combo)
        let bounds = Cap.bounds(of: caps)
        // The lit keys sit on the center line, so the name below lines up with them.
        let litMidX = Cap.bounds(of: caps.filter { $0.isLit(for: combo) }).midX
        let halfWidth = max(litMidX - bounds.minX, bounds.maxX - litMidX)
        ZStack {
            ForEach(caps) { cap in
                let isLit = cap.isLit(for: combo)
                KeyCap(cap: cap, isLit: isLit, isDown: press.holds(cap, isLit: isLit), unit: unit)
                    .frame(width: unit * cap.width, height: unit * cap.height)
                    .opacity(isLit ? 1 : cap.opacity)
                    .offset(
                        x: unit * (cap.x + cap.width / 2 - litMidX),
                        y: unit * (cap.y + cap.height / 2 - bounds.midY))
            }
        }
        .frame(width: unit * halfWidth * 2, height: unit * bounds.height)
        .frame(maxWidth: .infinity)
        .accessibilityElement()
        .accessibilityLabel(combo.map { "Shortcut, \(key.gesture(for: $0))" } ?? "Aria key, \(key.title)")
        .task(id: combo) { await showPress() }
    }

    /// A still drawing doesn't say "hold, then press": the Aria key goes
    /// down, then its partner, then both lift, a few times over.
    private func showPress() async {
        guard combo != nil, !reduceMotion else { return }
        for _ in 0..<3 {
            do {
                try await Task.sleep(for: .milliseconds(800))
                withAnimation(.easeOut(duration: 0.12)) { press = .ariaKeyDown }
                try await Task.sleep(for: .milliseconds(350))
                withAnimation(.easeOut(duration: 0.12)) { press = .bothDown }
                try await Task.sleep(for: .milliseconds(700))
                withAnimation(.easeOut(duration: 0.2)) { press = .up }
            } catch {
                press = .up
                return
            }
        }
    }
}

private enum Press {
    case up
    case ariaKeyDown
    case bothDown

    func holds(_ cap: Cap, isLit: Bool) -> Bool {
        switch self {
        case .up: false
        case .ariaKeyDown: cap.isAriaKey
        case .bothDown: isLit
        }
    }
}

private struct Cap: Identifiable {
    enum Face {
        /// A modifier: its symbol above its name.
        case modifier(symbol: String, name: String)
        /// A character key: its shifted mark over its plain one.
        case marks(String, String)
        case arrow(symbol: String)
        /// A key with nothing printed on it, like the space bar.
        case blank
    }

    let face: Face
    /// Key widths from the Aria key's top-left corner.
    let x: CGFloat
    let y: CGFloat
    var width: CGFloat = 1
    var height: CGFloat = 1
    var opacity: Double = 1
    var isAriaKey = false
    /// The combo this key makes when held with the Aria key.
    var completes: ShortcutCombo?

    var id: String { "\(x),\(y)" }

    func isLit(for combo: ShortcutCombo?) -> Bool {
        isAriaKey || (combo != nil && completes == combo)
    }

    /// The box around every key, in key widths.
    static func bounds(of caps: [Cap]) -> CGRect {
        caps.map { CGRect(x: $0.x, y: $0.y, width: $0.width, height: $0.height) }
            .reduce(CGRect.null) { $0.union($1) }
    }

    /// Every layout puts the Aria key at the origin. A combo's key joins the
    /// neighbors only when the drawing asks for it.
    static func layout(for key: AriaKey, combo: ShortcutCombo?) -> [Cap] {
        switch (key, combo) {
        case (.rightOption, .shift):
            // Right shift ends the row, so the left side gives up keys to keep the lit pair mid-drawing.
            [
                Cap(face: .arrow(symbol: "arrowtriangle.left.fill"), x: 1.12, y: 0.52, height: 0.48, opacity: 0.5),
                Cap(face: .arrow(symbol: "arrowtriangle.up.fill"), x: 2.24, y: 0, height: 0.46, opacity: 0.25),
                Cap(face: .arrow(symbol: "arrowtriangle.down.fill"), x: 2.24, y: 0.54, height: 0.46, opacity: 0.25),
                Cap(face: .arrow(symbol: "arrowtriangle.right.fill"), x: 3.36, y: 0.52, height: 0.48, opacity: 0.2),
                Cap(face: .marks(">", "."), x: -0.13, y: -1.12, opacity: 0.35),
                Cap(face: .marks("?", "/"), x: 0.99, y: -1.12, opacity: 0.5),
                Cap(face: .modifier(symbol: "shift", name: "shift"), x: 2.11, y: -1.12, width: 2.3, completes: .shift),
                Cap(face: .modifier(symbol: "option", name: "option"), x: 0, y: 0, isAriaKey: true),
            ]
        case (.fn, .shift):
            // Left shift is as wide as fn and control together, so the row stops there.
            [
                Cap(face: .modifier(symbol: "control", name: "control"), x: 1.12, y: 0, opacity: 0.5),
                Cap(face: .modifier(symbol: "shift", name: "shift"), x: 0, y: -1.12, width: 2.25, completes: .shift),
                Cap(face: .modifier(symbol: "globe", name: "fn"), x: 0, y: 0, isAriaKey: true),
            ]
        case (.rightOption, .command):
            // The space bar and M balance the arrows, so the lit pair sits mid-drawing.
            layout(for: key, combo: nil) + [
                Cap(face: .blank, x: -3.69, y: 0, width: 2.2, opacity: 0.25),
                Cap(face: .marks("M", ""), x: -2.37, y: -1.12, opacity: 0.2),
            ]
        case (.fn, .command):
            layout(for: key, combo: nil) + [
                Cap(
                    face: .modifier(symbol: "command", name: "command"), x: 3.36, y: 0, width: 1.25, completes: .command
                )
            ]
        case (.rightOption, _):
            [
                Cap(
                    face: .modifier(symbol: "command", name: "command"), x: -1.37, y: 0, width: 1.25, opacity: 0.5,
                    completes: .command),
                Cap(face: .arrow(symbol: "arrowtriangle.left.fill"), x: 1.12, y: 0.52, height: 0.48, opacity: 0.5),
                Cap(face: .arrow(symbol: "arrowtriangle.up.fill"), x: 2.24, y: 0, height: 0.46, opacity: 0.25),
                Cap(face: .arrow(symbol: "arrowtriangle.down.fill"), x: 2.24, y: 0.54, height: 0.46, opacity: 0.25),
                Cap(face: .marks("<", ","), x: -1.25, y: -1.12, opacity: 0.35),
                Cap(face: .marks(">", "."), x: -0.13, y: -1.12, opacity: 0.5),
                Cap(face: .marks("?", "/"), x: 0.99, y: -1.12, opacity: 0.35),
                Cap(face: .modifier(symbol: "option", name: "option"), x: 0, y: 0, isAriaKey: true),
            ]
        case (.fn, _):
            [
                Cap(face: .modifier(symbol: "control", name: "control"), x: 1.12, y: 0, opacity: 0.5),
                Cap(face: .modifier(symbol: "option", name: "option"), x: 2.24, y: 0, opacity: 0.25),
                Cap(
                    face: .modifier(symbol: "shift", name: "shift"), x: 0, y: -1.12, width: 2.25, opacity: 0.4,
                    completes: .shift),
                Cap(face: .marks("Z", ""), x: 2.37, y: -1.12, opacity: 0.2),
                Cap(face: .modifier(symbol: "globe", name: "fn"), x: 0, y: 0, isAriaKey: true),
            ]
        }
    }
}

private struct KeyCap: View {
    let cap: Cap
    let isLit: Bool
    let isDown: Bool
    let unit: CGFloat

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: unit * 0.15, style: .continuous)
        face
            .foregroundStyle(isLit ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
            .padding(.vertical, unit * 0.12)
            .padding(.horizontal, unit * 0.14)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background {
                if isLit {
                    shape.fill(.tint.opacity(isDown ? 0.35 : 0.15))
                } else {
                    shape.fill(.quaternary)
                }
            }
            .overlay {
                if isLit { shape.strokeBorder(.tint, lineWidth: 1.5) }
            }
            .scaleEffect(isDown ? 0.92 : 1)
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
        case .blank:
            Color.clear
        }
    }
}
