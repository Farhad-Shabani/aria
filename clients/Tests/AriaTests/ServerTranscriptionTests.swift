import XCTest

@testable import Aria

@MainActor
final class ServerTranscriptionTests: XCTestCase {
    private struct Refused: LocalizedError {
        var errorDescription: String? { "Refused." }
    }

    private final class Spy {
        var inspection: Aria_Setup_V1_Inspection
        var sentRunsOn: [RunsOn] = []
        var paired: [String] = []
        var pairFails = false
        var forgotten = 0
        var forgetFails = false
        var inspectFailsAfterSet = false
        var inspectFailsAfterPair = false
        var setFails = false
        /// Sent before a switch that then waits until cancelled.
        var downloadEvents: [Aria_Setup_V1_SetupEvent]?

        init(_ inspection: Aria_Setup_V1_Inspection) {
            self.inspection = inspection
        }

        var effects: ServerTranscription.Effects {
            .init(
                inspect: {
                    if self.inspectFailsAfterSet && !self.sentRunsOn.isEmpty { throw Refused() }
                    if self.inspectFailsAfterPair && !self.paired.isEmpty { throw Refused() }
                    return self.inspection
                },
                pair: { invitation in
                    self.paired.append(invitation)
                    if self.pairFails { throw Refused() }
                },
                forget: {
                    if self.forgetFails { throw Refused() }
                    self.forgotten += 1
                    self.inspection.clearServer()
                },
                setRunsOn: { runsOn, onEvent in
                    self.sentRunsOn.append(runsOn)
                    if self.setFails { throw Refused() }
                    if let events = self.downloadEvents {
                        events.forEach(onEvent)
                        try await Task.sleep(for: .seconds(60))
                    }
                    self.inspection.transcriptionRunsOn = runsOn.wire
                }
            )
        }
    }

    private static func mac(on runsOn: RunsOn) -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.defaults.transcriptionEngineID = "whisper"
        inspection.transcriptionRunsOn = runsOn.wire
        inspection.server.endpoint = "https://box:9851"
        return inspection
    }

    func testRunsOnIsChosenOnlyWithAPairingOrToLeaveTheServer() {
        let setup = SetupStatus()
        let server = ServerTranscription(
            effects: Spy(Self.mac(on: .local)).effects, setup: setup, runner: SetupRunner())
        var unpaired = Self.mac(on: .local)
        unpaired.clearServer()

        setup.show(unpaired)
        XCTAssertTrue(server.needsPairing)
        XCTAssertFalse(server.canChooseRunsOn)
        unpaired.transcriptionRunsOn = .server
        setup.show(unpaired)
        XCTAssertFalse(server.needsPairing)
        XCTAssertTrue(server.canChooseRunsOn)
        setup.show(Self.mac(on: .local))
        XCTAssertFalse(server.needsPairing)
        XCTAssertTrue(server.canChooseRunsOn)
    }

    func testSwitchingSendsOnlyRunsOnEachWay() async {
        let spy = Spy(Self.mac(on: .local))
        let setup = SetupStatus()
        let server = ServerTranscription(effects: spy.effects, setup: setup, runner: SetupRunner())

        await server.use(.server)
        XCTAssertEqual(setup.transcriptionRunsOn, .server)
        XCTAssertEqual(setup.server?.endpoint, "https://box:9851")

        await server.use(.local)
        XCTAssertEqual(spy.sentRunsOn, [.server, .local])
        XCTAssertEqual(setup.transcriptionRunsOn, .local)
        XCTAssertNil(server.failure)
    }

    private static func downloadEvents() -> [Aria_Setup_V1_SetupEvent] {
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .models
        var download = Aria_Setup_V1_SetupEvent()
        download.download.receivedBytes = 5
        download.download.totalBytes = 10
        return [started, download]
    }

    private func waitUntil(_ condition: () -> Bool) async {
        for _ in 0..<1000 where !condition() {
            await Task.yield()
        }
        XCTAssertTrue(condition())
    }

    func testComingBackShowsTheDownloadAndCancelEndsItQuietly() async {
        let spy = Spy(Self.mac(on: .server))
        spy.downloadEvents = Self.downloadEvents()
        let setup = SetupStatus()
        setup.show(spy.inspection)
        let server = ServerTranscription(effects: spy.effects, setup: setup, runner: SetupRunner())

        let switching = Task { await server.use(.local) }
        await waitUntil { server.progress == .downloading(ModelTransfer(received: 5, total: 10)) }
        server.cancel()
        await switching.value

        XCTAssertNil(server.progress)
        XCTAssertNil(server.failure)
        XCTAssertEqual(setup.transcriptionRunsOn, .server, "the cancelled switch wrote nothing")
    }

    func testAFailedSwitchSaysWhyAndKeepsTheShownRunsOn() async {
        let spy = Spy(Self.mac(on: .local))
        spy.setFails = true
        let setup = SetupStatus()
        setup.show(spy.inspection)
        let server = ServerTranscription(effects: spy.effects, setup: setup, runner: SetupRunner())

        await server.use(.server)

        XCTAssertEqual(server.failure, ServerTranscription.switchFailure)
        XCTAssertEqual(setup.transcriptionRunsOn, .local)
        XCTAssertFalse(server.isBusy)
    }

    func testASwitchThatLandedIsNotReportedAsFailed() async {
        let spy = Spy(Self.mac(on: .local))
        spy.inspectFailsAfterSet = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus(), runner: SetupRunner())

        await server.use(.server)

        XCTAssertEqual(spy.sentRunsOn, [.server])
        XCTAssertEqual(server.failure, ServerTranscription.readBackFailure)
    }

    func testPairingAloneLeavesTranscriptionOnYourMac() async {
        let spy = Spy(Self.mac(on: .local))
        let setup = SetupStatus()

        let paired = await ServerTranscription(effects: spy.effects, setup: setup, runner: SetupRunner()).pair(
            invitation: "{}")

        XCTAssertTrue(paired)
        XCTAssertEqual(spy.paired, ["{}"])
        XCTAssertEqual(spy.sentRunsOn, [])
        XCTAssertEqual(setup.transcriptionRunsOn, .local)
    }

    /// The invitation works once, so a pairing that landed must not ask
    /// for it again when only the read-back failed.
    func testAPairingThatLandedSucceedsEvenWhenItsReadBackFails() async {
        let spy = Spy(Self.mac(on: .local))
        spy.inspectFailsAfterPair = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus(), runner: SetupRunner())

        let paired = await server.pair(invitation: "{}")

        XCTAssertTrue(paired)
        XCTAssertNil(server.pairingFailure)
        XCTAssertEqual(server.failure, ServerTranscription.readBackFailure)
    }

    func testAFailedPairingShowsTheFallbackAndAppliesNothing() async {
        let spy = Spy(Self.mac(on: .server))
        spy.pairFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus(), runner: SetupRunner())

        let paired = await server.pair(invitation: "{}")

        XCTAssertFalse(paired)
        XCTAssertEqual(server.pairingFailure, ServerTranscription.pairFailure)
        XCTAssertNil(server.failure)
        XCTAssertEqual(spy.sentRunsOn, [])
        XCTAssertFalse(server.isBusy)
    }

    func testAKnownPairingFailureSaysWhatToDo() {
        func message(forReport json: String) -> String {
            ServerTranscription.pairingMessage(for: PairingFailure(report: Data(json.utf8)))
        }

        XCTAssertEqual(
            message(forReport: #"{"paired":false,"error":{"code":"permission_denied","message":"x"}}"#),
            "The server can’t pair more devices. Run `aria remote revoke` on the server to remove one, then try again.")
        XCTAssertEqual(
            message(forReport: #"{"paired":false,"error":{"code":"invitation_invalid","message":"x"}}"#),
            ServerTranscription.invitationFailure)
        XCTAssertEqual(
            message(forReport: #"{"paired":false,"error":{"code":"failed","message":"x"}}"#),
            ServerTranscription.pairFailure)
    }

    func testPairingWhileTheAppRunsAnotherSetupSaysItIsBusy() async {
        let spy = Spy(Self.mac(on: .local))
        let runner = SetupRunner()
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus(), runner: runner)
        let held = Task { try await runner.run { try await Task.sleep(for: .seconds(60)) } }
        while !runner.isRunning { await Task.yield() }

        let paired = await server.pair(invitation: "{}")
        held.cancel()

        XCTAssertFalse(paired)
        XCTAssertEqual(server.pairingFailure, SetupClientError.busyMessage)
        XCTAssertEqual(
            ServerTranscription.pairingMessage(
                for: PairingFailure(report: Data(#"{"paired":false,"error":{"code":"busy"}}"#.utf8))),
            SetupClientError.busyMessage)
    }

    func testAPairingCodeThisBuildDoesNotKnowIsAPlainFailure() {
        XCTAssertEqual(PairingFailure(report: Data(#"{"paired":false,"error":{"code":"new_code"}}"#.utf8)), .failed)
        XCTAssertEqual(PairingFailure(report: Data("error: not json".utf8)), .failed)
    }

    func testAnInvitationShowsItsAddressBeforePairing() {
        let invitation = #"{"endpoint":"https://box.local:9851","certificate_pem":"x","fingerprint":[],"code":"c"}"#
        XCTAssertEqual(ServerTranscription.invitationAddress(invitation), "box.local:9851")
        XCTAssertNil(ServerTranscription.invitationAddress("hello"))
        XCTAssertNil(ServerTranscription.invitationAddress(#"{"endpoint":"http://box:9851"}"#))
        XCTAssertEqual(
            ServerTranscription.invitationAddress(#"{"endpoint":"HTTPS://Box.local:9851/"}"#), "box.local:9851")
        XCTAssertEqual(ServerTranscription.address(of: "https://box.local"), "box.local")
    }

    func testOnlyAServerNotInUseCanBeForgotten() async {
        let remote = Spy(Self.mac(on: .server))
        let setupOnServer = SetupStatus()
        setupOnServer.show(remote.inspection)
        let onServer = ServerTranscription(effects: remote.effects, setup: setupOnServer, runner: SetupRunner())
        XCTAssertFalse(onServer.canForget)
        await onServer.forget()
        XCTAssertEqual(remote.forgotten, 0)

        let local = Spy(Self.mac(on: .local))
        let setup = SetupStatus()
        setup.show(local.inspection)
        let server = ServerTranscription(effects: local.effects, setup: setup, runner: SetupRunner())
        XCTAssertTrue(server.canForget)
        await server.forget()
        XCTAssertEqual(local.forgotten, 1)
        XCTAssertNil(setup.server)
        XCTAssertFalse(server.canForget)
    }

    func testAFailedForgetKeepsTheServerAndSaysWhy() async {
        let spy = Spy(Self.mac(on: .local))
        spy.forgetFails = true
        let setup = SetupStatus()
        setup.show(spy.inspection)
        let server = ServerTranscription(effects: spy.effects, setup: setup, runner: SetupRunner())

        await server.forget()

        XCTAssertEqual(server.failure, ServerTranscription.forgetFailure)
        XCTAssertNotNil(setup.server)
        XCTAssertFalse(server.isBusy)
    }

    func testPairingShowsTheAddressAndStartsOverAfterAFailure() async {
        let spy = Spy(Self.mac(on: .local))
        spy.pairFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus(), runner: SetupRunner())
        let invitation = #"{"endpoint":"https://box.local:9851"}"#

        server.paste("hello")
        XCTAssertEqual(server.pairingStep, .unreadable)
        server.paste(invitation)
        XCTAssertEqual(server.pairingStep, .confirming(invitation: invitation, address: "box.local:9851"))

        let paired = await server.pair(invitation: invitation)
        XCTAssertFalse(paired)
        XCTAssertEqual(server.pairingStep, .instructions)
        XCTAssertEqual(server.pairingFailure, ServerTranscription.pairFailure)

        server.paste(invitation)
        XCTAssertNil(server.pairingFailure, "a fresh invitation starts without the old error")
    }
}
