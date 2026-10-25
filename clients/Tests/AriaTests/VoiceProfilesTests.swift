import XCTest

@testable import Aria

@MainActor
final class VoiceProfilesTests: XCTestCase {
    private struct Enrolled: Equatable {
        let name: String
        let role: Aria_Daemon_V1_SpeakerRole
        let replace: Bool
    }

    @MainActor
    private final class Harness {
        var speakers = [Aria_Daemon_V1_EnrolledSpeaker]()
        var enrolled = [Enrolled]()
        var enrollError: Error?
        var enrollContinuation: CheckedContinuation<Void, Error>?
        var suspendsEnroll = false

        var effects: VoiceProfiles.Effects {
            .init(
                list: { self.speakers },
                enroll: { name, role, replace in
                    self.enrolled.append(Enrolled(name: name, role: role, replace: replace))
                    if let error = self.enrollError { throw error }
                    if self.suspendsEnroll {
                        try await withCheckedThrowingContinuation { self.enrollContinuation = $0 }
                    }
                },
                remove: { name in self.speakers.removeAll { $0.name == name } }
            )
        }
    }

    private static func speaker(_ name: String, _ role: Aria_Daemon_V1_SpeakerRole) -> Aria_Daemon_V1_EnrolledSpeaker {
        var speaker = Aria_Daemon_V1_EnrolledSpeaker()
        speaker.name = name
        speaker.role = role
        return speaker
    }

    private static func event(_ stage: Aria_Daemon_V1_SpeakerStage, index: UInt32) -> Aria_Daemon_V1_SpeakerEvent {
        var event = Aria_Daemon_V1_SpeakerEvent()
        event.stage = stage
        event.index = index
        event.samples = 5
        return event
    }

    private static func colors() -> VoiceColors {
        VoiceColors(defaults: UserDefaults(suiteName: "VoiceProfilesTests-\(UUID())")!)
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    func test_aNewVoiceIsTheOwnersUntilAnOwnerExists() async {
        let harness = Harness()
        harness.speakers = [Self.speaker("sam", .guest)]
        let voices = VoiceProfiles(effects: harness.effects, colors: Self.colors())
        await voices.refresh()

        voices.beginAdding()
        XCTAssertEqual(voices.enrollment?.role, .owner)
        XCTAssertEqual(voices.colors.color(of: "sam"), .blue, "a listed voice gets a color")

        harness.speakers.append(Self.speaker("alex", .owner))
        await voices.refresh()
        voices.beginAdding()
        XCTAssertEqual(voices.enrollment?.role, .guest)
    }

    func test_reRecordingKeepsTheRoleAndReplaces() async {
        let harness = Harness()
        let sam = Self.speaker("sam", .guest)
        harness.speakers = [sam]
        let voices = VoiceProfiles(effects: harness.effects, colors: Self.colors())
        await voices.refresh()

        voices.beginRerecording(sam)
        voices.start()
        await waitUntil { voices.enrollment == nil }

        XCTAssertEqual(harness.enrolled, [Enrolled(name: "sam", role: .guest, replace: true)])
    }

    func test_progressFollowsTheDaemonsSamples() {
        let voices = VoiceProfiles(effects: Harness().effects, colors: Self.colors())
        voices.beginAdding()
        voices.name = "alex"
        voices.speakerEvent(Self.event(.recording, index: 1))
        XCTAssertEqual(voices.enrollment?.phase, .naming, "events before Start belong to someone else")

        voices.start()
        voices.speakerEvent(Self.event(.recording, index: 2))
        XCTAssertEqual(voices.enrollment?.phase, .recording(taken: 1, listening: true))
        voices.speakerEvent(Self.event(.sampleTaken, index: 2))
        XCTAssertEqual(voices.enrollment?.phase, .recording(taken: 2, listening: false))
    }

    func test_aFailedRecordingReturnsToTheNameWithTheReason() async {
        let harness = Harness()
        harness.enrollError = TryOutError.stopped
        let voices = VoiceProfiles(effects: harness.effects, colors: Self.colors())
        voices.beginAdding()
        voices.name = " alex "
        voices.start()
        await waitUntil { voices.enrollment?.phase == .naming }

        XCTAssertEqual(harness.enrolled.map(\.name), ["alex"])
        XCTAssertEqual(voices.enrollment?.failure, TryOutError.stopped.localizedDescription)
    }

    func test_everySampleHasASentenceToRead() {
        XCTAssertEqual(VoiceProfiles.sentences.count, VoiceProfiles.samples)
    }

    func test_cancelClosesTheSheetAndIgnoresLateEvents() async {
        let harness = Harness()
        harness.suspendsEnroll = true
        let voices = VoiceProfiles(effects: harness.effects, colors: Self.colors())
        voices.beginAdding()
        voices.name = "alex"
        voices.start()
        await waitUntil { harness.enrollContinuation != nil }

        voices.cancel()
        harness.enrollContinuation?.resume(throwing: CancellationError())
        voices.speakerEvent(Self.event(.recording, index: 2))
        for _ in 0..<10 { await Task.yield() }

        XCTAssertNil(voices.enrollment)
        XCTAssertNil(voices.failure)
    }

    func test_theOwnVoiceToggleNamesWhoseVoiceItKeeps() {
        XCTAssertEqual(VoiceProfiles.ownVoiceSummary(owners: []), "Set up your voice to use this.")
        XCTAssertEqual(VoiceProfiles.ownVoiceSummary(owners: ["Farhad"]), "Types only Farhad’s voice.")
        XCTAssertEqual(
            VoiceProfiles.ownVoiceSummary(owners: ["Farhad", "Alex"]),
            "Types only Farhad’s and Alex’s voices."
        )
    }
}
