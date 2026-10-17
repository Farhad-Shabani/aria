import XCTest

@testable import Aria

@MainActor
final class SetupStateTests: XCTestCase {
    @MainActor
    private final class Harness {
        var inspection = SetupStateTests.freshMac()
        var applied: Aria_Setup_V1_ApplyRequest?
        var onEvent: (@Sendable (Aria_Setup_V1_SetupEvent) -> Void)?
        var applyError: Error?
        var applyContinuation: CheckedContinuation<Aria_Setup_V1_Summary, Error>?
        var stoppedDaemon = 0
        var startedDaemon = 0
        var armed = false
        var speakers = [Aria_Daemon_V1_EnrolledSpeaker]()
        var enrolledNames = [String]()
        var heard = "hello there"
        var spoken = [String]()
        var summary = SetupStateTests.summary(grant: .armed)

        var effects: SetupState.Effects {
            .init(
                inspect: { self.inspection },
                apply: { request, onEvent in
                    self.applied = request
                    self.onEvent = onEvent
                    if let error = self.applyError { throw error }
                    return try await withCheckedThrowingContinuation { self.applyContinuation = $0 }
                },
                stopDaemon: { self.stoppedDaemon += 1 },
                startDaemon: { self.startedDaemon += 1 },
                hotkeyArmed: { self.armed },
                openAccessibility: {},
                speakers: { self.speakers },
                enroll: { name in
                    self.enrolledNames.append(name)
                    var response = Aria_Daemon_V1_EnrollSpeakerResponse()
                    response.consistency = 0.9
                    return response
                },
                transcribe: { self.heard },
                speak: { self.spoken.append($0) },
                waitForGrant: { await Task.yield() }
            )
        }
    }

    private static func engine(_ key: String, default isDefault: Bool = false, unsupported: String? = nil)
        -> Aria_Setup_V1_Engine
    {
        var engine = Aria_Setup_V1_Engine()
        engine.key = key
        engine.label = key.capitalized
        engine.default = isDefault
        if let unsupported {
            engine.availability = .unsupported
            engine.note = unsupported
        } else {
            engine.availability = .ready
        }
        return engine
    }

    private static func model(
        _ name: String, engine: String = "", present: Bool = false, default isDefault: Bool = true
    )
        -> Aria_Setup_V1_Model
    {
        var model = Aria_Setup_V1_Model()
        model.name = name
        model.title = name.capitalized
        model.bytes = 10
        model.engine = engine
        model.present = present
        model.default = isDefault
        return model
    }

    /// Nothing configured, nothing on disk, no service.
    static func freshMac() -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.macos = true
        inspection.stt = [engine("whisper", default: true), engine("parakeet-mlx", unsupported: "needs MLX")]
        inspection.tts = [engine("system", default: true), engine("kokoro")]
        inspection.chat = [engine("claude-code", default: true)]
        inspection.defaults.stt = "whisper"
        inspection.defaults.tts = "system"
        inspection.defaults.chat = "claude-code"
        inspection.defaults.hotkey = true
        inspection.models = [
            model("whisper", engine: "whisper"),
            model("kokoro", engine: "kokoro"),
            model("speaker", default: false),
            model("segmentation"),
        ]
        inspection.service.job = .absent
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .config
        inspection.missing = [missing]
        return inspection
    }

    static func summary(grant: Aria_Setup_V1_Grant, voiceModel: Bool = true) -> Aria_Setup_V1_Summary {
        var summary = Aria_Setup_V1_Summary()
        summary.grant = grant
        summary.voiceModelPresent = voiceModel
        return summary
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<1_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("State did not settle", file: file, line: line)
    }

    private func walkToApplying(_ harness: Harness) async -> SetupState {
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        state.next()
        XCTAssertEqual(state.step, .choices)
        state.next()
        XCTAssertEqual(state.step, .models)
        state.next()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(state.step, .applying)
        return state
    }

    func test_express_appliesTheDefaultsWithoutTheChoicePages() async {
        let harness = Harness()
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        XCTAssertEqual(state.missingSummary, "Missing: configuration.")

        state.express()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(state.step, .applying)
        XCTAssertEqual(harness.applied?.selections, harness.inspection.defaults)
        XCTAssertEqual(harness.applied?.downloads, ["whisper", "segmentation"])

        state.cancel()
        harness.applyContinuation?.resume(throwing: SetupClientError.cancelled)
        await waitUntil { state.step == .welcome }
    }

    func test_pagesSlideForwardExceptOnBackAndCancel() async {
        let harness = Harness()
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        XCTAssertTrue(state.forward)
        state.next()
        state.next()
        XCTAssertTrue(state.forward)
        state.back()
        XCTAssertFalse(state.forward)
        state.next()
        XCTAssertTrue(state.forward)
    }

    func test_customize_walksBackToTheWelcomePage() async {
        let harness = Harness()
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }

        state.next()
        state.next()
        XCTAssertEqual(state.step, .models)
        state.back()
        state.back()
        XCTAssertEqual(state.step, .welcome)
    }

    func test_stagesAndOrder_followTheSteps() {
        let walk: [SetupState.Step] = [
            .inspecting, .welcome, .choices, .models, .applying, .grant, .voice, .tryOut, .done, .failed("x"),
        ]
        XCTAssertEqual(walk.map(\.order), [0, 0, 1, 2, 3, 4, 5, 6, 7, 8], "inspecting and welcome share a page")
        XCTAssertEqual(
            walk.map(\.stage),
            [nil, nil, .choose, .download, .install, .finish, .finish, .finish, .finish, nil]
        )
        XCTAssertNil(SetupState.Step.failed("x").caption)
        XCTAssertEqual(SetupState.Step.welcome.title, "Welcome to Aria")
    }

    func test_summaries_readAsSentences() async {
        let harness = Harness()
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        state.next()
        state.next()

        XCTAssertEqual(state.downloadSummary, "2 downloads, 20 bytes")
        state.downloads = []
        XCTAssertEqual(state.downloadSummary, "Nothing selected")
        state.downloads = ["whisper"]
        XCTAssertEqual(state.downloadSummary, "1 download, 10 bytes")

        XCTAssertEqual(state.installRows.map(\.progress), [.pending, .pending, .pending, .pending])
        state.next()
        await waitUntil { harness.applyContinuation != nil }
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .models
        harness.onEvent?(started)
        await waitUntil { state.applying == .models }
        XCTAssertEqual(state.installRows.map(\.progress), [.done, .done, .running, .pending])
        XCTAssertEqual(
            state.installRows.map(\.title), ["Configuration", "Wake-word models", "Models", "Background service"])
    }

    func test_freshMac_walksEveryStepToDone() async {
        let harness = Harness()
        let state = await walkToApplying(harness)

        XCTAssertEqual(state.selections.stt, "whisper")
        XCTAssertEqual(state.selections.tts, "system")
        XCTAssertTrue(state.selections.hotkey)
        XCTAssertFalse(state.selections.wake)
        XCTAssertEqual(harness.applied?.downloads, ["whisper", "segmentation"], "the speaker model is off by default")
        XCTAssertEqual(harness.stoppedDaemon, 1, "a spawned daemon must give the socket to the service")

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .voice }
        XCTAssertEqual(harness.startedDaemon, 0, "the service owns the daemon after a finished apply")

        state.voiceName = " Ada "
        state.recordVoice()
        await waitUntil { state.step == .tryOut }
        XCTAssertEqual(harness.enrolledNames, ["Ada"])
        XCTAssertEqual(state.enrolled, "Ada enrolled (consistency 0.90)")

        state.tryPress()
        await waitUntil { state.heard != nil }
        XCTAssertEqual(state.heard, "hello there")
        XCTAssertEqual(harness.spoken, [SetupState.spokenReply])
        state.next()
        XCTAssertEqual(state.step, .done)
    }

    func test_applyEvents_driveTheStepAndTheProgress() async {
        let harness = Harness()
        let state = await walkToApplying(harness)
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .models
        var download = Aria_Setup_V1_SetupEvent()
        download.download.model = "whisper"
        download.download.received = 5
        download.download.total = 10

        harness.onEvent?(started)
        harness.onEvent?(download)
        await waitUntil { state.progress["whisper"] != nil }

        XCTAssertEqual(state.applying, .models)
        XCTAssertEqual(state.progress["whisper"]?.received, 5)
    }

    func test_continueAnyway_leavesTheGrantPage() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        let state = await walkToApplying(harness)
        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .grant }

        state.next()
        await waitUntil { state.step == .voice }
    }

    func test_cancelDuringApply_returnsToModels() async {
        let harness = Harness()
        let state = await walkToApplying(harness)

        state.cancel()
        harness.applyContinuation?.resume(throwing: SetupClientError.cancelled)
        await waitUntil { state.step == .models }
        XCTAssertEqual(harness.startedDaemon, 1, "a cancelled apply must restart the daemon it stopped")
    }

    func test_applyFailure_isShownAndRetryReinspects() async {
        let harness = Harness()
        harness.applyError = SetupClientError.failed("curl exited with 22")
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        state.next()
        state.next()
        state.next()
        await waitUntil { state.step == .failed("curl exited with 22") }
        XCTAssertEqual(harness.startedDaemon, 1, "a failed apply must restart the daemon it stopped")

        state.retry()
        await waitUntil { state.step == .welcome }
    }

    func test_applyFailure_leavesAServiceDaemonAlone() async {
        let harness = Harness()
        harness.inspection.service.job = .running
        harness.applyError = SetupClientError.failed("curl exited with 22")
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        state.next()
        state.next()
        state.next()
        await waitUntil { state.step == .failed("curl exited with 22") }
        XCTAssertEqual(harness.stoppedDaemon, 0)
        XCTAssertEqual(harness.startedDaemon, 0, "the service supervises its own daemon")
    }

    func test_grantWaits_untilTheHotkeyArms() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        let state = await walkToApplying(harness)

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .grant }

        harness.armed = true
        await waitUntil { state.step == .voice }
    }

    func test_voiceIsSkipped_whenSomeoneIsEnrolledOrTheModelWasSkipped() async {
        let harness = Harness()
        var ada = Aria_Daemon_V1_EnrolledSpeaker()
        ada.name = "Ada"
        harness.speakers = [ada]
        let state = await walkToApplying(harness)
        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .tryOut }

        XCTAssertFalse(SetupState.offersVoice(Self.summary(grant: .armed, voiceModel: false), speakers: []))
        XCTAssertTrue(SetupState.offersVoice(Self.summary(grant: .armed), speakers: []))
    }

    /// The answers come from the daemon's own `--yes` rule, verbatim: an
    /// engine this Mac cannot run blocks the page, one setup does not
    /// know passes through.
    func test_configuredMac_startsFromItsAnswersAndHasNothingToDownload() async {
        var inspection = Self.freshMac()
        inspection.defaults.stt = "parakeet-mlx"
        inspection.defaults.tts = "kokoro"
        inspection.defaults.chat = "codex"
        inspection.models = inspection.models.map { model in
            var model = model
            model.present = true
            return model
        }
        inspection.missing = []
        let harness = Harness()
        harness.inspection = inspection
        let state = SetupState(effects: harness.effects)
        XCTAssertNil(state.missingSummary, "nothing to say before the Mac is looked at")
        state.start()
        await waitUntil { state.step == .welcome }
        XCTAssertEqual(state.missingSummary, "Everything is in place.")
        state.express()
        XCTAssertEqual(state.step, .welcome, "an engine this Mac cannot run blocks Express too")
        state.next()

        XCTAssertEqual(state.selections, inspection.defaults)
        XCTAssertTrue(SetupState.plan(inspection, state.selections).isEmpty)
        XCTAssertEqual(state.downloadSummary, "Nothing to download")
        XCTAssertEqual(state.blockedBy, "Parakeet-Mlx: needs MLX")
        state.next()
        XCTAssertEqual(state.step, .choices, "an engine this Mac cannot run does not pass")
        XCTAssertFalse(SetupState.shouldOffer(missing: inspection.missing, offeredBefore: false))
    }

    func test_planFollowsTheChosenEngines() {
        let inspection = Self.freshMac()
        var selections = inspection.defaults
        selections.tts = "kokoro"

        XCTAssertEqual(
            SetupState.plan(inspection, selections).map(\.name),
            ["whisper", "kokoro", "speaker", "segmentation"]
        )
    }

    func test_planSkipsAModelConfiguredElsewhere() {
        var inspection = Self.freshMac()
        inspection.models[0].external = true

        XCTAssertFalse(SetupState.plan(inspection, inspection.defaults).map(\.name).contains("whisper"))
    }

    func test_wizardIsOfferedOnce() {
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .grant
        XCTAssertTrue(SetupState.shouldOffer(missing: [missing], offeredBefore: false))
        XCTAssertFalse(SetupState.shouldOffer(missing: [missing], offeredBefore: true))
        XCTAssertFalse(SetupState.shouldOffer(missing: [], offeredBefore: false))
    }
}
