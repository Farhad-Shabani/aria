import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class PalettePromptFormTests: XCTestCase {
    private struct Host: View {
        @State var draft = PromptDraft()
        @FocusState var isNameFocused: Bool

        var body: some View {
            PalettePromptForm(draft: $draft, isNameFocused: $isNameFocused, chooseFile: {})
        }
    }

    func testTheFormFitsThePaletteWithoutScrolling() throws {
        let height = PalettePanel.size.height - PaletteBarMetrics.headerHeight - PaletteBarMetrics.footerHeight
        let host = NSHostingView(rootView: Host())
        host.frame = NSRect(x: 0, y: 0, width: PalettePanel.size.width, height: height)
        let window = NSWindow(contentRect: host.frame, styleMask: .borderless, backing: .buffered, defer: false)
        window.contentView = host
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        host.layoutSubtreeIfNeeded()

        let form = try XCTUnwrap(Self.outermostScrollView(in: host))
        let document = try XCTUnwrap(form.documentView).frame.height

        XCTAssertLessThanOrEqual(document, form.contentView.bounds.height)
    }

    private static func outermostScrollView(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView { return scroll }
        return view.subviews.lazy.compactMap(outermostScrollView).first
    }
}
