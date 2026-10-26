import AppKit
import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class PromptRowTests: XCTestCase {
    func testFileWarningRefreshesWhenWindowBecomesKey() throws {
        let directory = FileManager.default.temporaryDirectory.appending(
            path: UUID().uuidString, directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let file = directory.appending(path: "prompt.md")
        var entry = Aria_Daemon_V1_Prompt()
        entry.name = "Prompt"
        entry.instructionsPath = file.path

        let host = NSHostingView(rootView: PromptRow(entry: entry, flagsMissingFile: true))
        let window = NSWindow(
            contentRect: NSRect(x: -10000, y: -10000, width: 500, height: 160),
            styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        window.orderBack(nil)
        host.layoutSubtreeIfNeeded()

        let normalHeight = NSHostingView(rootView: PromptRow(entry: entry)).fittingSize.height
        wait(
            for: [
                XCTNSPredicateExpectation(
                    predicate: NSPredicate { _, _ in host.fittingSize.height > normalHeight + 5 }, object: nil)
            ], timeout: 3)

        try Data().write(to: file)
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window)
        wait(
            for: [
                XCTNSPredicateExpectation(
                    predicate: NSPredicate { _, _ in host.fittingSize.height <= normalHeight + 5 }, object: nil)
            ], timeout: 3)
    }
}
