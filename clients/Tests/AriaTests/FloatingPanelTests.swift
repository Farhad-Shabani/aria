import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class FloatingPanelTests: XCTestCase {
    func testOverlayStaysAbovePopupMenus() throws {
        let existing = Set(NSApp.windows.map(\.windowNumber))
        let overlay = FloatingPanel(width: 480, bottomMargin: 34)
        overlay.show(Text("typing"))
        let panel = try XCTUnwrap(
            NSApp.windows.first { !existing.contains($0.windowNumber) } as? NSPanel
        )
        defer { panel.orderOut(nil) }

        XCTAssertGreaterThan(panel.level.rawValue, NSWindow.Level.popUpMenu.rawValue)
    }
}
