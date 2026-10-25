import Carbon.HIToolbox
import XCTest

@testable import Aria

final class HotkeyTests: XCTestCase {
    private let chord = Chord(keyCode: 49, control: true, option: true, shift: false, command: false)
    private let other = Chord(keyCode: 35, control: true, option: false, shift: true, command: false)

    func testAHotkeyRoundTripsThroughTheWireAndAnUnusableChordReadsAsNone() {
        XCTAssertEqual(Aria_Daemon_V1_Hotkey(chord).storedChord, chord)
        XCTAssertNil(Aria_Daemon_V1_Hotkey(nil).storedChord)

        var bare = Aria_Daemon_V1_Hotkey(chord)
        bare.chord.control = false
        bare.chord.option = false
        XCTAssertNil(bare.storedChord)
    }

    func testAPromptWithNoChordRunsOnlyFromThePromptList() {
        var on = PromptEdits.makeEntry(for: URL(fileURLWithPath: "/on.md"))
        on.chord = chord.wire
        let bare = PromptEdits.makeEntry(for: URL(fileURLWithPath: "/bare.md"))
        var off = PromptEdits.makeEntry(for: URL(fileURLWithPath: "/off.md"))
        off.enabled = false
        off.chord = other.wire

        let plan = PromptChords([on, bare, off])

        XCTAssertEqual(plan.enabled, [on.id, bare.id])
        XCTAssertEqual(plan.chords, [on.id: chord])
    }

    func testTheListChordStandsInForTheRunItStartedAndOtherwiseTogglesTheList() {
        var gesture = PromptListGesture()
        XCTAssertEqual(gesture.press(isOpen: false) { _ in true }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)

        gesture.chose("started")
        XCTAssertEqual(gesture.press(isOpen: false) { $0 == "started" }, .prompt(id: "started"))
        XCTAssertEqual(gesture.press(isOpen: false) { _ in false }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)

        let entries = (1...10).map { PromptEdits.makeEntry(for: URL(fileURLWithPath: "/\($0).md")) }
        let keys = PromptListGesture.keys(for: entries)
        XCTAssertEqual(keys.map(\.entry.id), entries.prefix(9).map(\.id))
        XCTAssertEqual(keys.first?.keyCode, kVK_ANSI_1)
        XCTAssertEqual(keys.last?.keyCode, kVK_ANSI_9)
    }
}
