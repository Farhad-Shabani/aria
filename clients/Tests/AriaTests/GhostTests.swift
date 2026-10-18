import XCTest

@testable import Aria

final class GhostStateTests: XCTestCase {
    private func snapshot(text: String = "", placed: Bool = true) -> Aria_Daemon_V1_GhostSnapshot {
        var snapshot = Aria_Daemon_V1_GhostSnapshot()
        snapshot.text = text
        if placed {
            snapshot.placement.caret.height = 16
        }
        return snapshot
    }

    private func enabled() -> GhostState {
        var state = GhostState()
        state.setEnabled(true)
        return state
    }

    func test_placedWordsShowUntilTheyLand() {
        var state = enabled()
        state.apply(snapshot(text: "turn lift"), now: 0)
        XCTAssertTrue(state.isVisible)
        XCTAssertEqual(state.text, "turn lift")
        state.apply(snapshot(), now: 1)
        XCTAssertFalse(state.isVisible)
    }

    func test_wordsWithoutACaretShowNothing() {
        var state = enabled()
        state.apply(snapshot(text: "turn lift", placed: false), now: 0)
        XCTAssertFalse(state.isVisible)
    }

    func test_turningItOffClearsAndKeepsItHidden() {
        var state = enabled()
        state.apply(snapshot(text: "words"), now: 0)
        state.setEnabled(false)
        XCTAssertFalse(state.isVisible)
        state.apply(snapshot(text: "words"), now: 1)
        XCTAssertFalse(state.isVisible)
    }

    func test_aGhostLeftAloneExpires() {
        var state = enabled()
        state.apply(snapshot(text: "words"), now: 0)
        state.expire(now: GhostState.expiry - 0.1)
        XCTAssertTrue(state.isVisible)
        state.expire(now: GhostState.expiry)
        XCTAssertFalse(state.isVisible)
    }
}

final class GhostLayoutTests: XCTestCase {
    func test_aFieldWrapsTheGhostLikeItsOwnText() {
        let layout = GhostLayout(
            caret: CGRect(x: 160, y: 100, width: 0, height: 20),
            field: CGRect(x: 100, y: 90, width: 300, height: 200),
            primaryScreenHeight: 1000,
            ascender: 12,
            descender: -4
        )
        XCTAssertEqual(layout.firstLineIndent, 60)
        XCTAssertEqual(layout.lineHeight, 20)
        XCTAssertEqual(layout.baselineOffset, 2)
        // Six lines down from the caret line, flipped to a bottom-left origin.
        XCTAssertEqual(layout.frame, CGRect(x: 100, y: 780, width: 300, height: 120))
    }

    func test_aShortFieldStillHoldsOneLine() {
        let layout = GhostLayout(
            caret: CGRect(x: 160, y: 100, width: 0, height: 20),
            field: CGRect(x: 100, y: 95, width: 300, height: 20),
            primaryScreenHeight: 1000,
            ascender: 12,
            descender: -4
        )
        XCTAssertEqual(layout.frame.height, 20)
        XCTAssertEqual(layout.frame.maxY, 900)
    }

    func test_withoutAFieldTheGhostFloatsFromTheCaret() {
        let layout = GhostLayout(
            caret: CGRect(x: 160, y: 100, width: 0, height: 20),
            field: nil,
            primaryScreenHeight: 1000,
            ascender: 12,
            descender: -4
        )
        XCTAssertEqual(layout.firstLineIndent, 0)
        XCTAssertEqual(layout.frame.minX, 160)
        XCTAssertEqual(layout.frame.width, GhostLayout.floatingWidth)
        XCTAssertEqual(layout.frame.maxY, 900)
    }
}
