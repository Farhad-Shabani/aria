import XCTest

@testable import Aria

final class HeardSpeakerTests: XCTestCase {
    private func heard(_ heard: Aria_Daemon_V1_HeardSpeaker, _ speaker: String = "") -> Aria_Daemon_V1_DictationEvent {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .speakerChanged
        event.heard = heard
        event.speakerName = speaker
        return event
    }

    private func stage(_ stage: Aria_Daemon_V1_DictationStage) -> Aria_Daemon_V1_DictationEvent {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = stage
        return event
    }

    func testTheVoiceFollowsWhoHoldsTheFloor() {
        var voice = HeardSpeaker()
        voice.apply(stage(.recording))
        voice.apply(heard(.typed, "Farhad"))
        XCTAssertEqual(voice.voice, .named("Farhad"))

        voice.apply(heard(.undecided))
        XCTAssertNil(voice.voice)

        voice.apply(heard(.keptOut))
        XCTAssertEqual(voice.voice, .keptOut)

        voice.apply(heard(.typed))
        XCTAssertNil(voice.voice, "a voice nobody enrolled is not named")
    }

    func testATypedSegmentKeepsTheVoiceHeard() {
        var voice = HeardSpeaker()
        voice.apply(heard(.keptOut))
        var segment = stage(.segmentTranscribed)
        segment.speakerName = "Farhad"
        voice.apply(segment)
        XCTAssertEqual(voice.voice, .keptOut)
    }

    func testAStreamGapForgetsTheVoiceUntilItIsHeardAgain() {
        var voice = HeardSpeaker()
        voice.apply(heard(.typed, "Farhad"))
        voice.apply(nil)
        voice.apply(stage(.transcribing))
        XCTAssertNil(voice.voice)

        voice.apply(heard(.keptOut))
        XCTAssertEqual(voice.voice, .keptOut)
    }

    func testTheResultKeepsTheVoiceUntilTheNextPress() {
        var voice = HeardSpeaker()
        voice.apply(heard(.typed, "Farhad"))
        voice.apply(stage(.delivered))
        XCTAssertEqual(voice.voice, .named("Farhad"))

        voice.apply(stage(.failed))
        XCTAssertNil(voice.voice, "a press that fails before it records is unheard")
    }
}
