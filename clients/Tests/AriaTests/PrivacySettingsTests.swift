import SwiftUI
import XCTest

@testable import Aria

@MainActor
final class PrivacySettingsTests: XCTestCase {
    func testTurningMemoryOnAsksBeforeWriting() {
        var stored = false
        var asked = false
        let toggle = PrivacySettings.memorySwitch(
            Binding(get: { stored }, set: { stored = $0 }),
            confirm: { asked = true }
        )

        toggle.wrappedValue = true

        XCTAssertTrue(asked)
        XCTAssertFalse(stored)
    }

    func testTurningMemoryOffWritesAtOnce() {
        var stored = true
        var asked = false
        let toggle = PrivacySettings.memorySwitch(
            Binding(get: { stored }, set: { stored = $0 }),
            confirm: { asked = true }
        )

        toggle.wrappedValue = false

        XCTAssertFalse(asked)
        XCTAssertFalse(stored)
    }
}
