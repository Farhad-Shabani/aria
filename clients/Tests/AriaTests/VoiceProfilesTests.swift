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
        var setOwnerError: Error?
        var listContinuation: CheckedContinuation<Void, Never>?
        /// Makes the next list call wait after reading, like a slow socket.
        var suspendsNextList = false

        var effects: VoiceProfiles.Effects {
            .init(
                list: {
                    let speakers = self.speakers
                    if self.suspendsNextList {
                        self.suspendsNextList = false
                        await withCheckedContinuation { self.listContinuation = $0 }
                    }
                    return speakers
                },
                enroll: { name, role, replace in
                    self.enrolled.append(Enrolled(name: name, role: role, replace: replace))
                    if let error = self.enrollError { throw error }
                    if self.suspendsEnroll {
                        try await withCheckedThrowingContinuation { self.enrollContinuation = $0 }
                    }
                },
                remove: { name in self.speakers.removeAll { $0.name == name } },
                setOwner: { name in
                    if let error = self.setOwnerError { throw error }
                    for index in self.speakers.indices where self.speakers[index].role == .owner {
                        self.speakers[index].role = .guest
                    }
                    for index in self.speakers.indices where self.speakers[index].name == name {
                        self.speakers[index].role = .owner
                    }
                }
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

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    func testANewVoiceIsTheOwnersUntilAnOwnerExists() async {
        let harness = Harness()
        harness.speakers = [Self.speaker("sam", .guest)]
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()

        voices.beginAdding()
        XCTAssertEqual(voices.enrollment?.role, .owner)

        harness.speakers.append(Self.speaker("alex", .owner))
        await voices.refresh()
        voices.beginAdding()
        XCTAssertEqual(voices.enrollment?.role, .guest)
    }

    func testTheOwnVoiceToggleExplainsItselfOnlyOnceAVoiceExists() async {
        let harness = Harness()
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()
        XCTAssertNil(voices.ownVoiceSummary)

        harness.speakers = [Self.speaker("sam", .guest)]
        await voices.refresh()
        XCTAssertEqual(voices.ownVoiceSummary, "Make one voice the owner to use this.")

        harness.speakers = [Self.speaker("sam", .owner)]
        await voices.refresh()
        XCTAssertEqual(
            voices.ownVoiceSummary, "Types only your voice. The status pill glows when Aria hears you.")
    }

    func testAListReadBeforeARemovalDoesNotBringTheVoiceBack() async {
        let harness = Harness()
        harness.speakers = [Self.speaker("sam", .owner), Self.speaker("alex", .guest)]
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()
        harness.suspendsNextList = true
        let stale = Task { await voices.refresh() }
        await waitUntil { harness.listContinuation != nil }

        await voices.remove("alex")
        XCTAssertEqual(voices.speakers?.map(\.name), ["sam"])
        harness.listContinuation?.resume()
        await stale.value

        XCTAssertEqual(voices.speakers?.map(\.name), ["sam"])
    }

    func testMakingAGuestTheOwnerShowsBothNewRoles() async {
        let harness = Harness()
        harness.speakers = [Self.speaker("alex", .owner), Self.speaker("sam", .guest)]
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()

        await voices.makeOwner("sam")

        XCTAssertEqual(voices.speakers?.map(\.role), [.guest, .owner])
        XCTAssertNil(voices.failure)
    }

    func testAFailedOwnerChangeStillShowsTheDaemonsRoles() async {
        let harness = Harness()
        harness.speakers = [Self.speaker("alex", .owner), Self.speaker("sam", .guest)]
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()
        // The daemon promoted Sam, then failed to demote Alex.
        harness.speakers[1].role = .owner
        harness.setOwnerError = SpeakerFailure(errorDescription: "Aria couldn’t change the owner.")

        await voices.makeOwner("sam")

        XCTAssertEqual(voices.speakers?.map(\.role), [.owner, .owner])
        XCTAssertEqual(voices.failure, "Aria couldn’t change the owner.")
    }

    func testReRecordingKeepsTheRoleAndReplaces() async {
        let harness = Harness()
        let sam = Self.speaker("sam", .guest)
        harness.speakers = [sam]
        let voices = VoiceProfiles(effects: harness.effects)
        await voices.refresh()

        voices.beginRerecording(sam)
        voices.start()
        await waitUntil { voices.enrollment == nil }

        XCTAssertEqual(harness.enrolled, [Enrolled(name: "sam", role: .guest, replace: true)])
    }

    func testProgressFollowsTheDaemonsSamples() {
        let voices = VoiceProfiles(effects: Harness().effects)
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

    func testAFailedRecordingReturnsToTheNameWithTheReason() async {
        let harness = Harness()
        harness.enrollError = TryOutError.stopped
        let voices = VoiceProfiles(effects: harness.effects)
        voices.beginAdding()
        voices.name = " alex "
        voices.start()
        await waitUntil { voices.enrollment?.phase == .naming }

        XCTAssertEqual(harness.enrolled.map(\.name), ["alex"])
        XCTAssertEqual(voices.enrollment?.failure, TryOutError.stopped.localizedDescription)
    }

    func testEverySampleHasASentenceToRead() {
        XCTAssertEqual(VoiceProfiles.sentences.count, VoiceProfiles.samples)
    }

    func testCancelClosesTheSheetAndIgnoresLateEvents() async {
        let harness = Harness()
        harness.suspendsEnroll = true
        let voices = VoiceProfiles(effects: harness.effects)
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

    func testLateEnrollmentSuccessKeepsTheNextSheetAndRefreshesSavedVoices() async {
        let harness = Harness()
        harness.suspendsEnroll = true
        let voices = VoiceProfiles(effects: harness.effects)
        voices.beginAdding()
        voices.name = "alex"
        voices.start()
        await waitUntil { harness.enrollContinuation != nil }

        voices.cancel()
        voices.beginAdding()
        voices.name = "sam"
        harness.speakers = [Self.speaker("alex", .owner)]
        harness.enrollContinuation?.resume()
        await waitUntil { voices.speakers?.map(\.name) == ["alex"] }

        XCTAssertEqual(voices.enrollment?.phase, .naming)
        XCTAssertEqual(voices.name, "sam")
        XCTAssertNil(voices.failure)
    }
}
