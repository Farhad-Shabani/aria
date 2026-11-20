import SwiftUI

struct PairingCodeImage: View, Equatable {
    /// The white tile's side, quiet zone included. Q correction packs more
    /// modules than L, so the tile grows to keep each module as large.
    static let side: CGFloat = 192
    private static let quietZone: CGFloat = 14
    private static var codeSide: CGFloat { side - 2 * quietZone }

    let invite: PairingInvite

    var body: some View {
        if let code = invite.url.flatMap({ PairingCode($0.absoluteString) }) {
            // White in dark mode too, so the phone's camera still reads it.
            ZStack {
                Canvas { context, _ in Self.draw(code, in: &context) }
                HairlineMark(figure: MarkMotion.resting, lines: .small)
                    .frame(width: Self.markSide)
            }
            .foregroundStyle(.black)
            .frame(width: Self.codeSide, height: Self.codeSide)
            .padding(Self.quietZone)
            .background(.white, in: RoundedRectangle(cornerRadius: 12))
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Pairing code")
        }
    }

    /// An even number of points, so the centred mark starts on a whole point
    /// and its hairlines land on whole pixels.
    private static var markSide: CGFloat { (PairingCode.markShare * codeSide / 2).rounded() * 2 }

    private static func draw(_ code: PairingCode, in context: inout GraphicsContext) {
        let module = codeSide / Double(code.size)
        var dots = Path()
        for y in 0..<code.size {
            for x in 0..<code.size where code.isDot(x: x, y: y) {
                let cell = CGRect(x: Double(x) * module, y: Double(y) * module, width: module, height: module)
                dots.addRoundedRect(
                    in: cell.insetBy(dx: module * 0.04, dy: module * 0.04), cornerSize: corner(0.3, module))
            }
        }
        context.fill(dots, with: .foreground)
        var finders = Path()
        let side = Double(PairingCode.finderSide) * module
        for finder in code.finders {
            let outer = CGRect(x: Double(finder.x) * module, y: Double(finder.y) * module, width: side, height: side)
            finders.addRoundedRect(in: outer, cornerSize: corner(2.3, module))
            finders.addRoundedRect(in: outer.insetBy(dx: module, dy: module), cornerSize: corner(1.4, module))
            finders.addRoundedRect(in: outer.insetBy(dx: 2 * module, dy: 2 * module), cornerSize: corner(1, module))
        }
        context.fill(finders, with: .foreground, style: FillStyle(eoFill: true))
    }

    private static func corner(_ modules: Double, _ module: Double) -> CGSize {
        CGSize(width: modules * module, height: modules * module)
    }
}
