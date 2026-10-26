import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class ItemListRenderingTests: XCTestCase {
    func testSelectedRemovalShrinksListWithoutOpeningDetails() throws {
        var selection: String?
        var removed: String?
        var opened: String?
        let firstSelected = expectation(description: "First row selected")
        let middleSelected = expectation(description: "Middle row selected")
        let host = NSHostingView(
            rootView: ItemListFixture(
                selectionChanged: {
                    selection = $0
                    if $0 == "First" { firstSelected.fulfill() }
                    if $0 == "Middle" { middleSelected.fulfill() }
                }, removed: { removed = $0 }, opened: { opened = $0 }
            ))
        let window = NSWindow(
            contentRect: NSRect(x: -10000, y: -10000, width: 500, height: 300),
            styleMask: [.borderless], backing: .buffered, defer: false
        )
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        window.orderBack(nil)
        host.layoutSubtreeIfNeeded()
        let table = try XCTUnwrap(descendants(host).compactMap { $0 as? NSTableView }.first)
        wait(
            for: [
                XCTNSPredicateExpectation(
                    predicate: NSPredicate { _, _ in table.numberOfRows == 3 }, object: nil)
            ],
            timeout: 2)
        window.makeFirstResponder(table)
        let buttons = descendants(host).compactMap { $0 as? NSButton }
        let remove = try XCTUnwrap(buttons.last)
        XCTAssertFalse(remove.isEnabled)

        let arrowDown = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, characters: "\u{F701}",
                charactersIgnoringModifiers: "\u{F701}", isARepeat: false, keyCode: 125))
        table.keyDown(with: arrowDown)
        wait(for: [firstSelected], timeout: 2)
        table.keyDown(with: arrowDown)
        wait(for: [middleSelected], timeout: 2)
        host.layoutSubtreeIfNeeded()
        XCTAssertTrue(remove.isEnabled)
        let scroll = try XCTUnwrap(table.enclosingScrollView)
        let heightBeforeRemoval = scroll.contentSize.height

        remove.performClick(nil)

        XCTAssertEqual(removed, "Middle")
        XCTAssertNil(selection)
        XCTAssertNil(opened)
        wait(
            for: [
                XCTNSPredicateExpectation(
                    predicate: NSPredicate { _, _ in
                        table.numberOfRows == 2 && scroll.contentSize.height < heightBeforeRemoval
                    }, object: nil)
            ], timeout: 2)
        XCTAssertEqual(scroll.contentSize.height, table.rect(ofRow: 1).maxY, accuracy: 1)
    }

    private func descendants(_ view: NSView) -> [NSView] {
        [view] + view.subviews.flatMap { descendants($0) }
    }
}

private struct ItemListFixture: View {
    @State private var selected: String?
    @State private var items = ["First", "Middle", "Last"]
    let selectionChanged: (String?) -> Void
    let removed: (String) -> Void
    let opened: (String) -> Void

    var body: some View {
        ItemList(
            items: items, id: \.self, noun: "Item", empty: "Empty",
            selected: Binding(
                get: { selected },
                set: {
                    selected = $0
                    selectionChanged($0)
                }),
            canAdd: true, add: {},
            remove: { id in
                items.removeAll { $0 == id }
                removed(id)
            }, open: opened
        ) { Text($0) }
        .frame(width: 500, height: 300)
    }
}
