import CoreImage
import SwiftUI
import XCTest

@testable import Aria

final class PairingCodeTests: XCTestCase {
    private let invite = PairingInvite(
        instanceName: "Aria (Alex's MacBook Pro)", code: "c0de1234",
        fingerprint: PairingInvite.fingerprint(ofDER: Data([0x30, 0x82, 0x01, 0xff, 0xfe, 0x00, 0x7f])))

    @MainActor
    func testTheCodeReadsWithTheMarkOverItAtQCorrection() throws {
        let url = try XCTUnwrap(invite.url)
        let detector = try XCTUnwrap(
            CIDetector(
                ofType: CIDetectorTypeQRCode, context: nil, options: [CIDetectorAccuracy: CIDetectorAccuracyHigh]))
        for scale in [1.0, 2.0] {
            let renderer = ImageRenderer(content: PairingCodeImage(invite: invite))
            renderer.scale = scale
            let image = try XCTUnwrap(renderer.cgImage, "@\(scale)x")
            let feature = try XCTUnwrap(
                detector.features(in: CIImage(cgImage: image)).first as? CIQRCodeFeature, "@\(scale)x")
            XCTAssertEqual(feature.messageString, url.absoluteString, "@\(scale)x")
            XCTAssertEqual(feature.symbolDescriptor?.errorCorrectionLevel, .levelQ, "@\(scale)x")
        }
    }

    func testNoModuleTouchesTheMark() throws {
        let code = try XCTUnwrap(PairingCode(try XCTUnwrap(invite.url).absoluteString))
        let middle = Double(code.size) / 2
        var cleared = 0
        for y in 0..<code.size {
            for x in 0..<code.size {
                let dx = Double(x) + 0.5 - middle
                let dy = Double(y) + 0.5 - middle
                let nearestEdge = (dx * dx + dy * dy).squareRoot() - 0.5 * 2.squareRoot()
                if nearestEdge < code.markWidth / 2 {
                    cleared += 1
                    XCTAssertFalse(code.isDot(x: x, y: y), "(\(x), \(y))")
                }
            }
        }
        XCTAssertGreaterThan(cleared, 0)
    }
}
