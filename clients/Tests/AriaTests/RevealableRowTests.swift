import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class RevealableRowTests: XCTestCase {
    /// A page waiting for settings builds its rows late. The mark's clock
    /// must start then, not when search picked the row.
    func testARowBuiltAfterItsRevealStillScrollsIntoViewThenClears() throws {
        let page = RevealFixturePage()
        page.revealed = .memory
        let host = NSHostingView(rootView: RevealFixture(page: page))
        let window = NSWindow(
            contentRect: NSRect(x: -10000, y: -10000, width: 500, height: 300),
            styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        window.orderBack(nil)

        RunLoop.main.run(until: Date().addingTimeInterval(2.5))
        XCTAssertEqual(page.revealed, .memory)

        page.hasRows = true
        let scroll = try XCTUnwrap(descendants(host).compactMap { $0 as? NSScrollView }.first)
        wait(
            for: [
                XCTNSPredicateExpectation(
                    predicate: NSPredicate { _, _ in scroll.contentView.bounds.origin.y > 0 }, object: nil)
            ],
            timeout: 2)
        XCTAssertEqual(page.revealed, .memory)
        wait(
            for: [
                XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in page.revealed == nil }, object: nil)
            ],
            timeout: 3)
    }

    func testAFocusRequestIsTakenOnceItsRowAppears() {
        let page = RevealFixturePage()
        page.focusRequest = .memory
        let host = NSHostingView(rootView: RevealFixture(page: page))
        let window = NSWindow(
            contentRect: NSRect(x: -10000, y: -10000, width: 500, height: 300),
            styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        window.orderBack(nil)

        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(page.focusRequest, .memory)

        page.hasRows = true
        wait(
            for: [
                XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in page.focusRequest == nil }, object: nil)
            ],
            timeout: 2)
    }

    private func descendants(_ view: NSView) -> [NSView] {
        view.subviews + view.subviews.flatMap(descendants)
    }
}

@MainActor
private final class RevealFixturePage: ObservableObject {
    @Published var revealed: SettingsRow?
    @Published var focusRequest: SettingsRow?
    @Published var hasRows = false
}

private struct RevealFixture: View {
    @ObservedObject var page: RevealFixturePage

    var body: some View {
        ScrollViewReader { proxy in
            Form {
                ForEach(0..<40) { Text("Filler \($0)") }
                if page.hasRows {
                    Toggle(SettingsRow.memory.title, isOn: .constant(false))
                        .settingsRow(.memory)
                }
            }
            .formStyle(.grouped)
            .environment(\.settingsScroll, proxy)
        }
        .environment(\.revealedRow, $page.revealed)
        .environment(\.focusRequest, $page.focusRequest)
    }
}
