import XCTest

@testable import Aria

@MainActor
final class ServerTranscriptionTests: XCTestCase {
    private struct Refused: LocalizedError {
        var errorDescription: String? { "Refused." }
    }

    private final class Spy {
        var inspection: Aria_Setup_V1_Inspection
        var requests: [Aria_Setup_V1_ApplyRequest] = []
        var applied: [String] { requests.map(\.selections.transcriptionEngineID) }
        var lastLocal: String?
        var paired: [String] = []
        var daemon: [String] = []
        var pairFails = false
        var forgotten = 0
        var forgetFails = false
        var inspectFailsAfterApply = false
        var applyFails = false

        init(_ inspection: Aria_Setup_V1_Inspection) {
            self.inspection = inspection
        }

        var effects: ServerTranscription.Effects {
            .init(
                inspect: {
                    if self.inspectFailsAfterApply && !self.requests.isEmpty { throw Refused() }
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
                apply: { request in
                    self.requests.append(request)
                    if self.applyFails { throw Refused() }
                    self.inspection.defaults.transcriptionEngineID = request.selections.transcriptionEngineID
                },
                stopDaemon: { self.daemon.append("stop") },
                startDaemon: { self.daemon.append("start") },
                lastLocalEngine: { self.lastLocal },
                rememberLocalEngine: { self.lastLocal = $0 }
            )
        }
    }

    private static func engine(
        _ key: String, availability: Aria_Setup_V1_Availability = .ready, model: Aria_Setup_V1_ModelSource = .downloads
    ) -> Aria_Setup_V1_Choice {
        var engine = Aria_Setup_V1_Choice()
        engine.id = key
        engine.availability = availability
        engine.modelSource = model
        return engine
    }

    private static func model(_ engine: String, onDisk: Aria_Setup_V1_OnDisk) -> Aria_Setup_V1_Model {
        var model = Aria_Setup_V1_Model()
        model.id = engine
        model.engineID = engine
        model.onDisk = onDisk
        return model
    }

    /// Parakeet comes first but has no model, and the MLX engine would
    /// fetch one; Whisper's is on disk.
    private static func mac(runningOn stt: String) -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.transcriptionChoices = [
            engine("parakeet"),
            engine("parakeet-mlx", model: .fetchedOnFirstUse),
            engine("whisper"),
            engine("server", model: .remote),
        ]
        inspection.models = [model("parakeet", onDisk: .absent), model("whisper", onDisk: .complete)]
        inspection.defaults.transcriptionEngineID = stt
        inspection.defaults.synthesisEngineID = "system"
        inspection.server.endpoint = "https://box:9851"
        inspection.service.job = .idle
        return inspection
    }

    func testThisMacPrefersThePreviousEngineThenTheFirstWithItsModelOnDisk() {
        var mac = Self.mac(runningOn: "server")
        mac.transcriptionChoices.append(Self.engine("moonshine"))
        mac.models.append(Self.model("moonshine", onDisk: .complete))
        XCTAssertEqual(ServerTranscription.localEngine(in: mac, preferring: nil), "whisper")
        XCTAssertEqual(ServerTranscription.localEngine(in: mac, preferring: "moonshine"), "moonshine")
        XCTAssertEqual(ServerTranscription.localEngine(in: mac, preferring: "parakeet"), "whisper")

        mac.models = mac.models.map { model in
            var model = model
            model.onDisk = .partial
            return model
        }
        XCTAssertNil(ServerTranscription.localEngine(in: mac, preferring: nil))
    }

    func testALocationIsChosenOnlyWithAPairingOrToLeaveTheServer() {
        let setup = SetupStatus()
        let server = ServerTranscription(effects: Spy(Self.mac(runningOn: "whisper")).effects, setup: setup)
        var unpaired = Self.mac(runningOn: "whisper")
        unpaired.clearServer()

        setup.show(unpaired)
        XCTAssertFalse(server.canChooseLocation)
        unpaired.defaults.transcriptionEngineID = "server"
        setup.show(unpaired)
        XCTAssertTrue(server.canChooseLocation)
        setup.show(Self.mac(runningOn: "whisper"))
        XCTAssertTrue(server.canChooseLocation)
    }

    func testSwitchingAppliesTheEngineAndKeepsTheOtherChoices() async {
        let spy = Spy(Self.mac(runningOn: "whisper"))
        let setup = SetupStatus()
        let server = ServerTranscription(effects: spy.effects, setup: setup)

        await server.use(.server)
        XCTAssertEqual(setup.transcription, .server)
        XCTAssertEqual(setup.server?.endpoint, "https://box:9851")
        XCTAssertEqual(spy.requests.first?.selections.synthesisEngineID, "system")
        XCTAssertEqual(spy.requests.first?.downloads, [])
        XCTAssertEqual(spy.lastLocal, "whisper")

        await server.use(.thisMac)
        XCTAssertEqual(spy.applied, ["server", "whisper"])
        XCTAssertEqual(setup.transcription, .thisMac)
        XCTAssertNil(server.notice)
        XCTAssertEqual(spy.daemon, [], "a registered service restarts itself")
    }

    func testThisMacWithoutALocalModelAppliesNothingAndPointsToSetup() async {
        var inspection = Self.mac(runningOn: "server")
        inspection.models = [Self.model("parakeet", onDisk: .absent), Self.model("whisper", onDisk: .absent)]
        let spy = Spy(inspection)
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())

        await server.use(.thisMac)

        XCTAssertEqual(spy.applied, [])
        XCTAssertEqual(server.notice, .needsSetup)
    }

    func testAFailedApplyWithoutAServiceRestartsTheAppsDaemon() async {
        var inspection = Self.mac(runningOn: "whisper")
        inspection.service.job = .absent
        let spy = Spy(inspection)
        spy.applyFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())

        await server.use(.server)

        XCTAssertEqual(spy.daemon, ["stop", "start"])
        XCTAssertEqual(server.notice, .failed("Refused."))
        XCTAssertFalse(server.isBusy)
    }

    func testASwitchThatLandedIsNotReportedAsFailed() async {
        let spy = Spy(Self.mac(runningOn: "whisper"))
        spy.inspectFailsAfterApply = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())

        await server.use(.server)

        XCTAssertEqual(spy.applied, ["server"])
        XCTAssertEqual(server.notice, .failed("Saved, but Aria couldn’t read it back: Refused."))
    }

    func testPairingReappliesOnlyAServerAlreadyInUse() async {
        let local = Spy(Self.mac(runningOn: "whisper"))
        let pairedLocally = await ServerTranscription(effects: local.effects, setup: SetupStatus())
            .pair(invitation: "{}")
        XCTAssertTrue(pairedLocally)
        XCTAssertEqual(local.paired, ["{}"])
        XCTAssertEqual(local.applied, [])

        let remote = Spy(Self.mac(runningOn: "server"))
        let pairedRemotely = await ServerTranscription(effects: remote.effects, setup: SetupStatus())
            .pair(invitation: "{}")
        XCTAssertTrue(pairedRemotely)
        XCTAssertEqual(remote.applied, ["server"])
    }

    /// The invitation works once, so a pairing that landed must not ask
    /// for it again when only the follow-up apply failed.
    func testAPairingThatLandedSucceedsEvenWhenItsApplyFails() async {
        let spy = Spy(Self.mac(runningOn: "server"))
        spy.applyFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())

        let paired = await server.pair(invitation: "{}")

        XCTAssertTrue(paired)
        XCTAssertNil(server.pairingFailure)
        XCTAssertEqual(server.notice, .failed("Refused."))
    }

    func testAFailedPairingShowsTheReasonAndAppliesNothing() async {
        let spy = Spy(Self.mac(runningOn: "server"))
        spy.pairFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())

        let paired = await server.pair(invitation: "{}")

        XCTAssertFalse(paired)
        XCTAssertEqual(server.pairingFailure, "Refused.")
        XCTAssertNil(server.notice)
        XCTAssertEqual(spy.applied, [])
        XCTAssertFalse(server.isBusy)
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
        let remote = Spy(Self.mac(runningOn: "server"))
        let setupOnServer = SetupStatus()
        setupOnServer.show(remote.inspection)
        let onServer = ServerTranscription(effects: remote.effects, setup: setupOnServer)
        XCTAssertFalse(onServer.canForget)
        await onServer.forget()
        XCTAssertEqual(remote.forgotten, 0)

        let local = Spy(Self.mac(runningOn: "whisper"))
        let setup = SetupStatus()
        setup.show(local.inspection)
        let server = ServerTranscription(effects: local.effects, setup: setup)
        XCTAssertTrue(server.canForget)
        await server.forget()
        XCTAssertEqual(local.forgotten, 1)
        XCTAssertNil(setup.server)
        XCTAssertFalse(server.canForget)
    }

    func testAFailedForgetKeepsTheServerAndSaysWhy() async {
        let spy = Spy(Self.mac(runningOn: "whisper"))
        spy.forgetFails = true
        let setup = SetupStatus()
        setup.show(spy.inspection)
        let server = ServerTranscription(effects: spy.effects, setup: setup)

        await server.forget()

        XCTAssertEqual(server.notice, .failed("Refused."))
        XCTAssertNotNil(setup.server)
        XCTAssertFalse(server.isBusy)
    }

    func testPairingShowsTheAddressAndStartsOverAfterAFailure() async {
        let spy = Spy(Self.mac(runningOn: "whisper"))
        spy.pairFails = true
        let server = ServerTranscription(effects: spy.effects, setup: SetupStatus())
        let invitation = #"{"endpoint":"https://box.local:9851"}"#

        server.paste("hello")
        XCTAssertEqual(server.pairingStep, .unreadable)
        server.paste(invitation)
        XCTAssertEqual(server.pairingStep, .confirming(invitation: invitation, address: "box.local:9851"))

        let paired = await server.pair(invitation: invitation)
        XCTAssertFalse(paired)
        XCTAssertEqual(server.pairingStep, .instructions)
        XCTAssertEqual(server.pairingFailure, "Refused.")

        server.paste(invitation)
        XCTAssertNil(server.pairingFailure, "a fresh invitation starts without the old error")
    }
}
