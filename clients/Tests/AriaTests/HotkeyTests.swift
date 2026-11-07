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
        var unknown = Aria_Daemon_V1_Shortcut()
        unknown.combo = .unspecified
        XCTAssertNil(unknown.storedBinding)
        unknown.combo = .UNRECOGNIZED(9)
        XCTAssertNil(unknown.storedBinding)

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
        XCTAssertEqual(ComboAction(combo: .command, in: settings), .palette)

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

    func testAPromptWithNoChordRunsOnlyFromThePalette() {
        var on = Aria_Daemon_V1_Prompt.stub(file: "/on.md")
        on.chord = chord.wire
        let bare = Aria_Daemon_V1_Prompt.stub(file: "/bare.md")
        var off = Aria_Daemon_V1_Prompt.stub(file: "/off.md")
        off.enabled = false
        off.chord = other.wire

        let plan = PromptChords([on, bare, off])

        XCTAssertEqual(plan.enabled, [on.id, bare.id])
        XCTAssertEqual(plan.chords, [on.id: chord])
    }
}
