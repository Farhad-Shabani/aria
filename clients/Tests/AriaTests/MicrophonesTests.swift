import XCTest

@testable import Aria

@MainActor
final class MicrophonesTests: XCTestCase {
    private let builtIn = ListedMicrophone(id: "coreaudio:built-in", label: "MacBook Pro Microphone")
    private let airPods = ListedMicrophone(id: "coreaudio:airpods", label: "AirPods Pro")

    private func list(_ microphones: [ListedMicrophone], current: String?) -> MicrophoneList {
        MicrophoneList(microphones: microphones, currentID: current)
    }

    func testAutomaticComesFirstThenEachConnectedDevice() {
        let choices = Microphones.choices(
            listed: list([builtIn, airPods], current: airPods.id), chosen: Microphones.automatic, names: [:])

        XCTAssertEqual(choices.map(\.title), ["Automatic", "MacBook Pro Microphone", "AirPods Pro"])
    }

    func testAChosenDeviceThatIsGoneStaysByItsRememberedName() {
        let choices = Microphones.choices(
            listed: list([builtIn], current: builtIn.id), chosen: airPods.id,
            names: [airPods.id: "AirPods Pro"])

        XCTAssertEqual(choices.last, Microphones.Choice(id: airPods.id, title: "AirPods Pro (Not Connected)"))
    }

    func testAChosenDeviceNeverSeenIsAnUnknownMicrophone() {
        let choices = Microphones.choices(listed: list([builtIn], current: builtIn.id), chosen: airPods.id, names: [:])

        XCTAssertEqual(choices.last?.title, "Unknown Microphone (Not Connected)")
    }

    func testAFailureSaysWhatWentWrongAndWhatToDo() {
        XCTAssertEqual(Microphones.failureMessage(DaemonFailure.unavailable), DaemonFailure.offlineMessage)
        XCTAssertEqual(
            Microphones.failureMessage(DaemonFailure.refused(.captureFailed)),
            "Aria couldn’t list your microphones. Check them in System Settings > Sound.")
        XCTAssertEqual(Microphones.failureMessage(DaemonFailure.timeout), "Something went wrong. Try again.")
    }

    func testBeforeTheFirstListTheChoiceShowsWithoutAState() {
        let choices = Microphones.choices(listed: nil, chosen: airPods.id, names: [airPods.id: "AirPods Pro"])

        XCTAssertEqual(choices.map(\.title), ["Automatic", "AirPods Pro"])
    }

    func testTheCaptionNamesWhatRecordsWhenTheChoiceDoesNot() {
        let listed = list([builtIn], current: builtIn.id)

        XCTAssertEqual(
            Microphones.caption(listed: listed, chosen: Microphones.automatic, names: [:]),
            "Records with MacBook Pro Microphone.")
        XCTAssertNil(Microphones.caption(listed: listed, chosen: builtIn.id, names: [:]))
        XCTAssertEqual(
            Microphones.caption(listed: listed, chosen: airPods.id, names: [airPods.id: "AirPods Pro"]),
            "AirPods Pro isn’t connected, so Aria records with MacBook Pro Microphone.")
        XCTAssertEqual(
            Microphones.caption(listed: list([], current: nil), chosen: Microphones.automatic, names: [:]),
            "No microphone is connected.")
    }

    func testARefreshRemembersEachDevicesName() async throws {
        let suite = "MicrophonesTests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let listed = list([airPods], current: airPods.id)
        let microphones = Microphones(fetch: { listed }, defaults: defaults)

        await microphones.refresh()

        XCTAssertEqual(microphones.list, listed)
        XCTAssertEqual(microphones.names, [airPods.id: "AirPods Pro"])
    }
}
