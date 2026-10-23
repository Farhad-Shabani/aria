import CoreImage.CIFilterBuiltins
import SwiftUI

struct PairingCodeImage: View, Equatable {
    /// The white tile's side, quiet zone included.
    static let side: CGFloat = 160
    private static let quietZone: CGFloat = 12

    let invite: PairingInvite

    var body: some View {
        if let code = qrCode {
            // White in dark mode too, so the phone's camera still reads it.
            Image(nsImage: code)
                .interpolation(.none)
                .resizable()
                .frame(width: Self.side - 2 * Self.quietZone, height: Self.side - 2 * Self.quietZone)
                .padding(Self.quietZone)
                .background(.white, in: RoundedRectangle(cornerRadius: 12))
                .accessibilityLabel("Pairing code")
        }
    }

    private var qrCode: NSImage? {
        guard let text = invite.url?.absoluteString else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "L"
        guard let image = filter.outputImage else { return nil }
        let representation = NSCIImageRep(ciImage: image)
        let result = NSImage(size: representation.size)
        result.addRepresentation(representation)
        return result
    }
}
