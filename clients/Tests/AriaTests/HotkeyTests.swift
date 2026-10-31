import Carbon.HIToolbox
import XCTest

@testable import Aria

final class HotkeyTests: XCTestCase {
    private let chord = Chord(keyCode: 49, control: true, option: true, shift: false, command: false)
    private let other = Chord(keyCode: 35, control: true, option: false, shift: true, command: false)

    func testAHotkeyRoundTripsThroughTheWireAndAnUnusableBindingReadsAsNone() {
        XCTAssertEqual(Aria_Daemon_V1_Shortcut(.chord(chord)).storedBinding, .chord(chord))
        XCTAssertEqual(Aria_Daemon_V1_Shortcut(.chord(chord)).storedChord, chord)
        XCTAssertEqual(Aria_Daemon_V1_Shortcut(.combo(.command)).storedBinding, .combo(.command))
        XCTAssertNil(Aria_Daemon_V1_Shortcut(.combo(.command)).storedChord)
        XCTAssertNil(Aria_Daemon_V1_Shortcut(nil).storedBinding)
        XCTAssertNil(Aria_Daemon_V1_Shortcut(.combo(.unspecified)).storedBinding)
        XCTAssertNil(Aria_Daemon_V1_Shortcut(.combo(.UNRECOGNIZED(9))).storedBinding)

        var bare = Aria_Daemon_V1_Shortcut(.chord(chord))
        bare.chord.control = false
        bare.chord.option = false
        XCTAssertNil(bare.storedBinding)
    }

    func testAComboRunsTheActionThatHoldsItAndShowsItsModifier() {
        var settings = Aria_Daemon_V1_Settings()
        settings.askShortcut = Aria_Daemon_V1_Shortcut(.combo(.shift))
        settings.promptListShortcut = Aria_Daemon_V1_Shortcut(.combo(.command))
        XCTAssertEqual(ComboAction(combo: .shift, in: settings), .ask)
        XCTAssertEqual(ComboAction(combo: .command, in: settings), .promptList)

        settings.askShortcut = Aria_Daemon_V1_Shortcut(nil)
        XCTAssertNil(ComboAction(combo: .shift, in: settings))
        settings.promptListShortcut = Aria_Daemon_V1_Shortcut(nil)
        XCTAssertNil(ComboAction(combo: .command, in: settings))

        settings.askShortcut = Aria_Daemon_V1_Shortcut(.combo(.command))
        settings.promptListShortcut = Aria_Daemon_V1_Shortcut(.chord(chord))
        XCTAssertEqual(ComboAction(combo: .command, in: settings), .ask)

        XCTAssertEqual(AriaKey.rightOption.gesture(for: .shift), "Right Option + ⇧")
        XCTAssertEqual(AriaKey.fn.gesture(for: .command), "Fn + ⌘")
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
