import Carbon.HIToolbox
import XCTest

@testable import Aria

final class HotkeyTests: XCTestCase {
    private let chord = Chord(keyCode: 49, control: true, option: true, shift: false, command: false)
    private let other = Chord(keyCode: 35, control: true, option: false, shift: true, command: false)

    func testAHotkeyRoundTripsThroughTheWireAndAnUnusableBindingReadsAsNone() {
        XCTAssertEqual(Aria_Daemon_V1_Hotkey(.chord(chord)).storedBinding, .chord(chord))
        XCTAssertEqual(Aria_Daemon_V1_Hotkey(.chord(chord)).storedChord, chord)
        XCTAssertEqual(Aria_Daemon_V1_Hotkey(.combo(.command)).storedBinding, .combo(.command))
        XCTAssertNil(Aria_Daemon_V1_Hotkey(.combo(.command)).storedChord)
        XCTAssertNil(Aria_Daemon_V1_Hotkey(nil).storedBinding)
        XCTAssertNil(Aria_Daemon_V1_Hotkey(.combo(.unspecified)).storedBinding)
        XCTAssertNil(Aria_Daemon_V1_Hotkey(.combo(.UNRECOGNIZED(9))).storedBinding)

        var bare = Aria_Daemon_V1_Hotkey(.chord(chord))
        bare.chord.control = false
        bare.chord.option = false
        XCTAssertNil(bare.storedBinding)
    }

    func testAComboRunsTheActionThatHoldsItAndShowsItsModifier() {
        var preferences = Aria_Daemon_V1_UserPreferences()
        preferences.askHotkey = Aria_Daemon_V1_Hotkey(.combo(.shift))
        preferences.promptListHotkey = Aria_Daemon_V1_Hotkey(.combo(.command))
        XCTAssertEqual(ComboAction(combo: .shift, in: preferences), .ask)
        XCTAssertEqual(ComboAction(combo: .command, in: preferences), .promptList)

        preferences.askHotkey = Aria_Daemon_V1_Hotkey(nil)
        XCTAssertNil(ComboAction(combo: .shift, in: preferences))
        preferences.promptListHotkey = Aria_Daemon_V1_Hotkey(nil)
        XCTAssertNil(ComboAction(combo: .command, in: preferences))

        preferences.askHotkey = Aria_Daemon_V1_Hotkey(.combo(.command))
        preferences.promptListHotkey = Aria_Daemon_V1_Hotkey(.chord(chord))
        XCTAssertEqual(ComboAction(combo: .command, in: preferences), .ask)

        XCTAssertEqual(HotkeyKey.rightOption.gesture(for: .shift), "Right Option + ⇧")
        XCTAssertEqual(HotkeyKey.fn.gesture(for: .command), "Fn + ⌘")
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

    func testTheListChordIsIgnoredWhileItsRunGoesAndOtherwiseTogglesTheList() {
        var gesture = PromptListGesture()
        XCTAssertEqual(gesture.press(isOpen: false) { _ in true }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)

        gesture.chose("started")
        XCTAssertEqual(gesture.press(isOpen: false) { $0 == "started" }, .ignore)
        XCTAssertEqual(gesture.press(isOpen: false) { _ in false }, .open)
        XCTAssertEqual(gesture.press(isOpen: true) { _ in true }, .close)

        let entries = (1...10).map { PromptEdits.makeEntry(for: URL(fileURLWithPath: "/\($0).md")) }
        let keys = PromptListGesture.keys(for: entries)
        XCTAssertEqual(keys.map(\.entry.id), entries.prefix(9).map(\.id))
        XCTAssertEqual(keys.first?.keyCode, kVK_ANSI_1)
        XCTAssertEqual(keys.last?.keyCode, kVK_ANSI_9)
    }
}
