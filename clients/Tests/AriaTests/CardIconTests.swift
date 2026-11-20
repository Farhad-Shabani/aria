import AppKit
import XCTest

@testable import Aria

final class CardIconTests: XCTestCase {
    func testSidebarCardIsDrawnForEachScaleAt44Points() {
        let card = NSImage(resource: .cardIcon)
        XCTAssertEqual(card.size, NSSize(width: 44, height: 44))
        XCTAssertEqual(Set(card.representations.map(\.pixelsWide)), [44, 88])
    }
}
