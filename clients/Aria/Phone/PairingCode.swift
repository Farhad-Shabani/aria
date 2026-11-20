import CoreImage.CIFilterBuiltins

/// A QR code's modules, with a disc in the middle cleared for the logo.
struct PairingCode {
    /// Modules a side.
    let size: Int
    private let dark: [Bool]

    /// Q recovers a quarter of the code. The cleared disc takes about 6%,
    /// which leaves room for a misread module or two.
    static let correctionLevel = "Q"
    /// The logo's width, as a share of the code's.
    static let markShare = 0.24
    /// Modules between the logo's edge and the nearest module's centre.
    private static let clearance = 1.1
    /// Each finder pattern's side, in modules.
    static let finderSide = 7

    init?(_ text: String) {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = Self.correctionLevel
        guard let image = filter.outputImage else { return nil }
        let width = Int(image.extent.width)
        let height = Int(image.extent.height)
        var pixels = [UInt8](repeating: 0, count: width * height * 4)
        CIContext().render(
            image, toBitmap: &pixels, rowBytes: width * 4, bounds: image.extent, format: .RGBA8,
            colorSpace: CGColorSpaceCreateDeviceRGB())
        let isDark = { (x: Int, y: Int) in pixels[(y * width + x) * 4] < 128 }
        // The generator adds its own quiet zone. The finder patterns fix the code's corners.
        let rows = (0..<height).filter { y in (0..<width).contains { isDark($0, y) } }
        let columns = (0..<width).filter { x in (0..<height).contains { isDark(x, $0) } }
        guard let top = rows.first, let left = columns.first, let bottom = rows.last, let right = columns.last,
            bottom - top == right - left
        else { return nil }
        let size = bottom - top + 1
        self.size = size
        dark = (0..<size * size).map { isDark(left + $0 % size, top + $0 / size) }
    }

    /// The logo's width, in modules.
    var markWidth: Double { Self.markShare * Double(size) }

    /// The top-left module of each finder pattern.
    var finders: [(x: Int, y: Int)] {
        let far = size - Self.finderSide
        return [(0, 0), (far, 0), (0, far)]
    }

    /// Whether the module at `x`, `y` is drawn on its own: dark, and in no
    /// finder pattern or the logo's disc.
    func isDot(x: Int, y: Int) -> Bool {
        guard dark[y * size + x], !isInFinder(x: x, y: y) else { return false }
        let middle = Double(size) / 2
        let dx = Double(x) + 0.5 - middle
        let dy = Double(y) + 0.5 - middle
        return (dx * dx + dy * dy).squareRoot() >= markWidth / 2 + Self.clearance
    }

    private func isInFinder(x: Int, y: Int) -> Bool {
        finders.contains { x >= $0.x && x < $0.x + Self.finderSide && y >= $0.y && y < $0.y + Self.finderSide }
    }
}
