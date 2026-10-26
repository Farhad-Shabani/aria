import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class ItemListRenderingTests: XCTestCase {
    func testSelectionFillsGroupedRowWithoutChangingItsLayout() throws {
        let (baseline, originalFrames) = try render(selected: nil)
        let first = try XCTUnwrap(originalFrames["First"])
        let last = try XCTUnwrap(originalFrames["Last"])
        let outside = try color(baseline, at: .zero)
        let left = CGFloat(
            try XCTUnwrap(
                (0..<Int(first.minX)).first {
                    try difference(outside, color(baseline, at: CGPoint(x: CGFloat($0), y: first.midY))) > 0.01
                }))
        let top = CGFloat(
            try XCTUnwrap(
                (0..<Int(first.minY)).first {
                    try difference(outside, color(baseline, at: CGPoint(x: 250, y: CGFloat($0)))) > 0.01
                }))
        let inset = first.minY - top
        for selected in ["First", "Middle", "Last"] {
            let (bitmap, frames) = try render(selected: selected)
            XCTAssertEqual(frames, originalFrames)
            let row = try XCTUnwrap(frames[selected])
            let center = CGPoint(x: 250, y: row.midY)
            let fill = try color(bitmap, at: center)
            XCTAssertGreaterThan(try difference(fill, color(baseline, at: center)), 0.1)

            for point in [
                CGPoint(x: left + 1, y: row.midY),
                CGPoint(x: 500 - left - 1, y: row.midY),
                CGPoint(x: 250, y: row.minY - inset + 1),
                CGPoint(x: 250, y: row.maxY + inset - 1),
            ] {
                XCTAssertLessThan(try difference(fill, color(bitmap, at: point)), 0.02)
            }
            for point in [
                CGPoint(x: left - 1, y: row.midY),
                CGPoint(x: 500 - left + 1, y: row.midY),
                CGPoint(x: left + 1, y: top + 1),
                CGPoint(x: left + 1, y: last.maxY + inset - 1),
            ] {
                XCTAssertLessThan(try difference(color(bitmap, at: point), color(baseline, at: point)), 0.02)
            }
        }
    }

    private func render(selected: String?) throws -> (NSBitmapImageRep, [String: CGRect]) {
        var frames: [String: CGRect] = [:]
        let form = Form {
            ItemList(
                items: ["First", "Middle", "Last"], id: \.self, noun: "Item", empty: "Empty",
                selected: .constant(selected), canAdd: true, add: {}, remove: { _ in }, open: { _ in }
            ) { item in
                Text(item)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .frame(height: 24)
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named("fixture"))
                    } action: {
                        frames[item] = $0
                    }
            }
        }
        .formStyle(.grouped)
        .coordinateSpace(name: "fixture")
        .frame(width: 500, height: 300)
        let host = NSHostingView(rootView: form)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 500, height: 300),
            styleMask: [.borderless], backing: .buffered, defer: false
        )
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        host.appearance = NSAppearance(named: .aqua)
        host.layoutSubtreeIfNeeded()
        let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: bitmap)
        return (bitmap, frames)
    }

    private func color(_ bitmap: NSBitmapImageRep, at point: CGPoint) throws -> NSColor {
        let scale = CGFloat(bitmap.pixelsWide) / 500
        return try XCTUnwrap(
            bitmap.colorAt(x: Int(point.x * scale), y: Int(point.y * scale))?.usingColorSpace(.deviceRGB))
    }

    private func difference(_ lhs: NSColor, _ rhs: NSColor) -> CGFloat {
        max(
            abs(lhs.redComponent - rhs.redComponent), abs(lhs.greenComponent - rhs.greenComponent),
            abs(lhs.blueComponent - rhs.blueComponent))
    }
}
