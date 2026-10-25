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
        var launchAtLogin = 0
        var installedCommandLineTool = 0
        var commandLineToolProblem: String?
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
                launchAtLogin: {
                    self.launchAtLogin += 1
                    return nil
                },
                installCommandLineTool: {
                    self.installedCommandLineTool += 1
                    return self.commandLineToolProblem
                },
                waitForGrant: { await Task.yield() }
            )
        }
    }

    private static func engine(
        _ key: String, default isDefault: Bool = false, unsupported: String? = nil,
        model: Aria_Setup_V1_ModelSource = .downloads
    ) -> Aria_Setup_V1_Engine {
        var engine = Aria_Setup_V1_Engine()
        engine.key = key
        engine.label = key.capitalized
        engine.default = isDefault
        engine.model = model
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
        model.onDisk = present ? .complete : .absent
        model.default = isDefault
        return model
    }

    /// Nothing configured, nothing on disk, no service.
    static func freshMac() -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.macos = true
        inspection.stt = [engine("parakeet", default: true), engine("parakeet-mlx", unsupported: "needs MLX")]
        inspection.tts = [engine("system", default: true), engine("kokoro")]
        inspection.chat = [engine("claude-code", default: true)]
        inspection.defaults.stt = "parakeet"
        inspection.defaults.tts = "system"
        inspection.defaults.chat = "claude-code"
        inspection.defaults.hotkey = true
        inspection.models = [
            model("parakeet", engine: "parakeet"),
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

    private func welcome(_ harness: Harness) async -> SetupState {
        let state = SetupState(effects: harness.effects)
        state.start()
        await waitUntil { state.step == .welcome }
        return state
    }

    private func walkToApplying(_ harness: Harness) async -> SetupState {
        let state = await welcome(harness)
        state.next()
        XCTAssertEqual(state.step, .choices)
        state.next()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(state.step, .applying)
        return state
    }

    func testExpressAppliesTheDefaultsWithoutTheChoicePages() async {
        let harness = Harness()
        let state = await welcome(harness)
        XCTAssertEqual(state.missingSummary, "Missing: configuration.")

        state.express()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(state.step, .applying)
        XCTAssertEqual(harness.applied?.selections, harness.inspection.defaults)
        XCTAssertEqual(harness.applied?.downloads, ["parakeet", "segmentation"])

        state.cancel()
        harness.applyContinuation?.resume(throwing: SetupClientError.cancelled)
        await waitUntil { state.step == .welcome }
    }

    func testPagesSlideForwardExceptOnBackAndCancel() async {
        let harness = Harness()
        let state = await welcome(harness)
        XCTAssertTrue(state.isMovingForward)
        state.next()
        XCTAssertTrue(state.isMovingForward)
        state.back()
        XCTAssertFalse(state.isMovingForward)
        state.next()
        XCTAssertTrue(state.isMovingForward)
    }

    func testChangingEnginesStartsTheDownloadPicksOver() async {
        let harness = Harness()
        let state = await welcome(harness)
        state.next()
        state.downloads = []

        state.selections.tts = "kokoro"

        XCTAssertEqual(state.downloads, ["parakeet", "kokoro", "segmentation"])
    }

    func testCustomizeWalksBackToTheWelcomePage() async {
        let harness = Harness()
        let state = await welcome(harness)

        state.next()
        XCTAssertEqual(state.step, .choices)
        state.back()
        XCTAssertEqual(state.step, .welcome)
    }

    func testStagesAndOrderFollowTheSteps() {
        let walk: [SetupState.Step] = [
            .inspecting, .welcome, .choices, .applying, .grant, .voice, .tryOut, .done, .failed("x"),
        ]
        XCTAssertEqual(walk.map(\.order), [0, 0, 1, 2, 3, 4, 5, 6, 7], "inspecting and welcome share a page")
        XCTAssertEqual(
            walk.map(\.stage),
            [.welcome, .welcome, .engines, .install, .hotkey, .voice, .tryOut, nil, nil]
        )
        XCTAssertNil(SetupState.Step.failed("x").caption)
        XCTAssertEqual(SetupState.Step.welcome.title, "Welcome to Aria")
    }

    func testSummariesReadAsSentences() async {
        let harness = Harness()
        let state = await welcome(harness)
        state.next()

        XCTAssertEqual(state.downloadTotal, "Total download: 20 bytes")
        state.downloads = []
        XCTAssertEqual(state.downloadTotal, "Nothing selected.")
        state.downloads = ["speaker"]
        XCTAssertEqual(state.downloadTotal, "Total download: 10 bytes")

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

    func testFreshMacWalksEveryStepToDone() async {
        let harness = Harness()
        harness.commandLineToolProblem = "The aria command was not installed."
        let state = await walkToApplying(harness)

        XCTAssertEqual(state.selections.stt, "parakeet")
        XCTAssertEqual(state.selections.tts, "system")
        XCTAssertTrue(state.selections.hotkey)
        XCTAssertFalse(state.selections.wake)
        XCTAssertEqual(harness.applied?.downloads, ["parakeet", "segmentation"], "the speaker model is off by default")
        XCTAssertEqual(harness.stoppedDaemon, 1, "a spawned daemon must give the socket to the service")

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .voice }
        XCTAssertEqual(harness.startedDaemon, 0, "the service owns the daemon after a finished apply")

        state.voiceName = " Ada "
        state.recordVoice()
        await waitUntil { state.step == .tryOut }
        XCTAssertEqual(harness.enrolledNames, ["Ada"])
        XCTAssertEqual(state.enrolled, "Voice saved for Ada.")

        XCTAssertEqual(harness.launchAtLogin, 0, "login starts only once setup finishes")
        state.tryPress()
        await waitUntil { state.step == .done }
        XCTAssertEqual(state.heard, "hello there")
        XCTAssertEqual(harness.spoken, [SetupState.spokenReply])
        XCTAssertEqual(harness.launchAtLogin, 1)
        XCTAssertEqual(harness.installedCommandLineTool, 1)
        XCTAssertEqual(state.notice, "The aria command was not installed.", "the Done page says what did not land")
    }

    func testApplyEventsDriveTheStepAndTheProgress() async {
        let harness = Harness()
        let state = await walkToApplying(harness)
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .models
        var download = Aria_Setup_V1_SetupEvent()
        download.download.model = "parakeet"
        download.download.received = 5
        download.download.total = 10

        harness.onEvent?(started)
        harness.onEvent?(download)
        await waitUntil { state.downloadProgress["parakeet"] != nil }

        XCTAssertEqual(state.applying, .models)
        XCTAssertEqual(state.downloadProgress["parakeet"]?.received, 5)
    }

    func testContinueAnywayLeavesTheGrantPage() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        let state = await walkToApplying(harness)
        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .grant }

        state.next()
        await waitUntil { state.step == .voice }
    }

    func testCancelDuringApplyReturnsToTheChoices() async {
        let harness = Harness()
        let state = await walkToApplying(harness)

        state.cancel()
        harness.applyContinuation?.resume(throwing: SetupClientError.cancelled)
        await waitUntil { state.step == .choices }
        XCTAssertEqual(harness.startedDaemon, 1, "a cancelled apply must restart the daemon it stopped")
    }

    func testApplyFailureIsShownAndRetryReinspects() async {
        let harness = Harness()
        harness.applyError = SetupClientError.failed("curl exited with 22")
        let state = await welcome(harness)
        state.next()
        state.next()
        await waitUntil { state.step == .failed("curl exited with 22") }
        XCTAssertEqual(harness.startedDaemon, 1, "a failed apply must restart the daemon it stopped")

        state.retry()
        await waitUntil { state.step == .welcome }
    }

    func testApplyFailureLeavesAServiceDaemonAlone() async {
        let harness = Harness()
        harness.inspection.service.job = .running
        harness.applyError = SetupClientError.failed("curl exited with 22")
        let state = await welcome(harness)
        state.next()
        state.next()
        await waitUntil { state.step == .failed("curl exited with 22") }
        XCTAssertEqual(harness.stoppedDaemon, 0)
        XCTAssertEqual(harness.startedDaemon, 0, "the service supervises its own daemon")
    }

    func testGrantWaitsUntilTheHotkeyArms() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        let state = await walkToApplying(harness)

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .grant }
        XCTAssertFalse(state.isHotkeyReady)

        harness.armed = true
        await waitUntil { state.step == .voice }
        XCTAssertTrue(state.isHotkeyReady, "Done teaches the hotkey once it arms")
    }

    func testVoiceIsSkippedWhenSomeoneIsEnrolledOrTheModelWasSkipped() async {
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
    func testConfiguredMacStartsFromItsAnswersAndHasNothingToDownload() async {
        var inspection = Self.freshMac()
        inspection.defaults.stt = "parakeet-mlx"
        inspection.defaults.tts = "kokoro"
        inspection.defaults.chat = "codex"
        inspection.models = inspection.models.map { model in
            var model = model
            model.onDisk = .complete
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
        XCTAssertEqual(state.downloadTotal, "No downloads needed.")
        XCTAssertEqual(state.blockedBy, "Parakeet-Mlx: needs MLX")
        state.next()
        XCTAssertEqual(state.step, .choices, "an engine this Mac cannot run does not pass")
        XCTAssertFalse(SetupState.shouldOffer(missing: inspection.missing, offeredBefore: false))
    }

    func testPlanFollowsTheChosenEngines() {
        let inspection = Self.freshMac()
        var selections = inspection.defaults
        selections.tts = "kokoro"

        XCTAssertEqual(
            SetupState.plan(inspection, selections).map(\.name),
            ["parakeet", "kokoro", "speaker", "segmentation"]
        )
    }

    func testPlanSkipsAModelConfiguredElsewhere() {
        var inspection = Self.freshMac()
        inspection.models[0].external = true

        XCTAssertFalse(SetupState.plan(inspection, inspection.defaults).map(\.name).contains("parakeet"))
    }

    func testExpressPreviewMatchesTheDownloadRequest() async {
        var inspection = Self.freshMac()
        inspection.models[3].onDisk = .complete
        let harness = Harness()
        harness.inspection = inspection
        let state = await welcome(harness)
        let preview = SetupState.expressPlan(state.inspection, state.selections)

        XCTAssertEqual(preview.map(\.name), ["parakeet"], "the installed and the optional are left out")
        XCTAssertEqual(SetupState.total(preview), "Total download: 10 bytes")
        state.express()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(harness.applied?.downloads, preview.map(\.name))
    }

    func testTotalNeverClaimsAnExactSizeForAResumedDownload() {
        var resumed = Self.model("parakeet", engine: "parakeet")
        resumed.onDisk = .partial

        XCTAssertEqual(SetupState.total([]), "No downloads needed.")
        XCTAssertEqual(SetupState.total([Self.model("speaker")]), "Total download: 10 bytes")
        XCTAssertEqual(SetupState.total([resumed, Self.model("speaker")]), "Total download: up to 20 bytes")
    }

    func testEngineDownloadTellsWhatPickingItFetches() {
        var inspection = Self.freshMac()
        inspection.models[1].onDisk = .partial
        var nemotron = Self.model("nemotron", engine: "nemotron")
        nemotron.external = true
        inspection.models += [Self.model("moonshine", engine: "moonshine", present: true), nemotron]
        let download = { (key: String) in SetupState.download(of: Self.engine(key), in: inspection) }
        let source = { (model: Aria_Setup_V1_ModelSource) in
            SetupState.download(of: Self.engine("other", model: model), in: inspection)
        }

        XCTAssertEqual(download("parakeet"), .size("10 bytes"))
        XCTAssertEqual(download("parakeet").note, "Download size: 10 bytes")
        XCTAssertNil(source(.builtIn).note, "the picker already shows a status")
        XCTAssertEqual(source(.builtIn), .status("Built in"))
        XCTAssertEqual(download("moonshine"), .status("Downloaded"))
        XCTAssertEqual(download("nemotron"), .status("Custom model"))
        XCTAssertEqual(download("kokoro").brief, "up to 10 bytes")
        XCTAssertEqual(source(.fetchedOnFirstUse), .status("Fetched on first use"))
        XCTAssertEqual(source(.remote), .status("On the server"))
        XCTAssertEqual(source(.unspecified), .status("Size unknown"), "never shown as nothing to download")
    }

    func testSelfFetchingEngineIsNotedOutsideTheTotal() async {
        var inspection = Self.freshMac()
        inspection.stt[1].availability = .missing
        inspection.stt[1].model = .fetchedOnFirstUse
        inspection.defaults.stt = "parakeet-mlx"
        let harness = Harness()
        harness.inspection = inspection
        let state = await welcome(harness)

        XCTAssertEqual(state.selfFetchingNotes, ["Parakeet-Mlx downloads its model on first use."])
        XCTAssertEqual(SetupState.expressPlan(state.inspection, state.selections).map(\.name), ["segmentation"])
    }

    func testWizardIsOfferedOnce() {
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .grant
        XCTAssertTrue(SetupState.shouldOffer(missing: [missing], offeredBefore: false))
        XCTAssertFalse(SetupState.shouldOffer(missing: [missing], offeredBefore: true))
        XCTAssertFalse(SetupState.shouldOffer(missing: [], offeredBefore: false))
    }
}
