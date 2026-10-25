import Carbon.HIToolbox
import XCTest

@testable import Aria

final class ChordCaptureTests: XCTestCase {
    func testDeleteAloneClearsTheChord() {
        XCTAssertEqual(Chord.captured(keyCode: UInt16(kVK_Delete), modifiers: []), .cleared)
        XCTAssertEqual(Chord.captured(keyCode: UInt16(kVK_ForwardDelete), modifiers: []), .cleared)
    }

    func testDeleteWithAModifierIsAChord() {
        let captured = Chord.captured(keyCode: UInt16(kVK_Delete), modifiers: [.control, .option])
        guard case .chord(let chord) = captured else {
            return XCTFail("\(captured)")
        }
        XCTAssertEqual(chord.keyCode, UInt16(kVK_Delete))
    }
}
