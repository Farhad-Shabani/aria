import AppKit
import XCTest

@testable import Aria

@MainActor
final class DockPresenceTests: XCTestCase {
    private func window(_ style: NSWindow.StyleMask = .titled) -> NSWindow {
        let window = NSWindow(contentRect: .zero, styleMask: style, backing: .buffered, defer: true)
        window.isReleasedWhenClosed = false
        addTeardownBlock { @MainActor in window.orderOut(nil) }
        return window
    }

    func testAShownTitledWindowNeedsTheDockIcon() {
        let shown = window()
        shown.orderFront(nil)
        XCTAssertTrue(DockPresence.needsDockIcon(windows: [shown]))
    }

    func testAClosedWindowNeedsNoDockIcon() {
        let closed = window()
        closed.orderFront(nil)
        closed.close()
        XCTAssertFalse(DockPresence.needsDockIcon(windows: [closed]))
    }

    func testAMinimizedWindowKeepsTheDockIcon() {
        final class MinimizedWindow: NSWindow {
            override var isMiniaturized: Bool { true }
        }
        let minimized = MinimizedWindow(contentRect: .zero, styleMask: .titled, backing: .buffered, defer: true)
        XCTAssertTrue(DockPresence.needsDockIcon(windows: [minimized]))
    }

    func testPanelsAndUntitledWindowsNeedNoDockIcon() {
        let panel = NSPanel(contentRect: .zero, styleMask: .titled, backing: .buffered, defer: true)
        addTeardownBlock { @MainActor in panel.orderOut(nil) }
        let untitled = window(.borderless)
        panel.orderFront(nil)
        untitled.orderFront(nil)
        XCTAssertFalse(DockPresence.needsDockIcon(windows: [panel, untitled]))
    }
}
