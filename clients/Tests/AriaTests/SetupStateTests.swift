import XCTest

@testable import Aria

@MainActor
final class SetupStateTests: XCTestCase {
    private struct Refused: Error {}

    @MainActor
    private final class Harness {
        var inspection = SetupStateTests.freshMac()
        var applied: Aria_Setup_V1_ApplyRequest?
        var onEvent: (@Sendable (Aria_Setup_V1_SetupEvent) -> Void)?
        var applyError: Error?
        var applyContinuation: CheckedContinuation<Aria_Setup_V1_Summary, Error>?
        var stoppedDaemon = 0
        var startedDaemon = 0
        var ariaKey = AriaKeyState.needsAccessibility
        var openedFor = [AriaKeyState]()
        var speakers = [EnrolledSpeaker]()
        var enrolledNames = [String]()
        var enrollError: Error?
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
                ariaKeyState: { self.ariaKey },
                openPrivacySettings: { self.openedFor.append($0) },
                speakers: { self.speakers },
                enroll: { name in
                    self.enrolledNames.append(name)
                    if let error = self.enrollError { throw error }
                },
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
    ) -> Aria_Setup_V1_Choice {
        var engine = Aria_Setup_V1_Choice()
        engine.id = key
        engine.label = key.capitalized
        engine.default = isDefault
        engine.modelSource = model
        if let unsupported {
            engine.availability = .unsupported
            engine.availabilityDetail = unsupported
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
        model.id = name
        model.title = name.capitalized
        model.sizeBytes = 10
        model.engineID = engine
        model.files = present ? .complete : .absent
        model.downloadByDefault = isDefault
        return model
    }

    /// Nothing configured, nothing on disk, no service.
    static func freshMac() -> Aria_Setup_V1_Inspection {
        var inspection = Aria_Setup_V1_Inspection()
        inspection.macos = true
        inspection.sttChoices = [
            engine("parakeet", default: true), engine("parakeet-mlx", unsupported: "needs MLX"),
        ]
        inspection.ttsChoices = [engine("system", default: true), engine("kokoro")]
        inspection.defaults.sttEngineID = "parakeet"
        inspection.defaults.ttsEngineID = "system"
        inspection.models = [
            model("parakeet", engine: "parakeet"),
            model("kokoro", engine: "kokoro"),
            model("speaker", default: false),
            model("segmentation"),
        ]
        inspection.service.job = .absent
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .settings
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
        let state = SetupState(effects: harness.effects, runner: SetupRunner())
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

    private func walkToVoice(_ harness: Harness) async -> SetupState {
        let state = await walkToApplying(harness)
        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .voice }
        return state
    }

    func testModelTitleFallsBackToItsKey() async {
        let state = await welcome(Harness())
        XCTAssertEqual(state.title(ofModel: "speaker"), "Speaker")
        XCTAssertEqual(state.title(ofModel: "unknown"), "unknown")
    }

    func testExpressAppliesTheDefaultsWithoutTheChoicePages() async {
        let harness = Harness()
        let state = await welcome(harness)
        XCTAssertEqual(state.missingSummary, "Missing: settings.")
        XCTAssertEqual(state.expressTotal, "Total download: 20 bytes")

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

        state.selections.ttsEngineID = "kokoro"

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
            .inspecting, .welcome, .choices, .applying, .grant, .voice, .tryOut, .openAria, .done, .failed("x"),
        ]
        XCTAssertEqual(
            walk.map(\.order), [0, 0, 1, 2, 3, 4, 5, 6, 6, 7],
            "inspecting and welcome share a page, as do Open Aria and done")
        XCTAssertEqual(
            walk.map(\.stage),
            [.welcome, .welcome, .engines, .install, .hotkey, .voice, .tryOut, .openAria, nil, nil]
        )
        XCTAssertNil(SetupState.Step.failed("x").caption)
        XCTAssertEqual(SetupState.Step.welcome.title, "Welcome to Aria")
    }

    func testTheRailNamesEachStepAndThePageSaysWhatToDo() {
        XCTAssertEqual(
            SetupProgress.Stage.allCases.map(\.title),
            ["Welcome", "Engines", "Installation", "Access", "Voice", "Dictation", "Shortcut"])
        let pages: [SetupState.Step] = [.welcome, .choices, .applying, .grant, .voice, .tryOut, .openAria]
        XCTAssertEqual(
            pages.map(\.title),
            [
                "Welcome to Aria", "Choose Your Engines", "Installing…", "Allow Access", "Set Up Your Voice",
                "Try Dictation", "Open Aria",
            ])
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

        XCTAssertEqual(state.installRows.map(\.progress), [.pending, .pending, .pending])
        state.next()
        await waitUntil { harness.applyContinuation != nil }
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .settings
        harness.onEvent?(started)
        await waitUntil { state.applying == .settings }
        started.started = .wakeword
        harness.onEvent?(started)
        XCTAssertEqual(state.installRows.map(\.progress), [.running, .pending, .pending])
        started.started = .models
        harness.onEvent?(started)
        await waitUntil { state.applying == .models }
        XCTAssertEqual(state.installRows.map(\.progress), [.done, .running, .pending])
        XCTAssertEqual(
            state.installRows.map(\.title), ["Settings", "Models", "Background service"])
    }

    func testFreshMacWalksEveryStepToDone() async {
        let harness = Harness()
        harness.commandLineToolProblem = "The aria command was not installed."
        let state = await walkToApplying(harness)

        XCTAssertEqual(state.selections.sttEngineID, "parakeet")
        XCTAssertEqual(state.selections.ttsEngineID, "system")
        XCTAssertFalse(state.selections.wake)
        XCTAssertEqual(harness.applied?.downloads, ["parakeet", "segmentation"], "the speaker model is off by default")
        XCTAssertEqual(harness.stoppedDaemon, 1, "a spawned daemon must give the socket to the service")
        XCTAssertEqual(harness.launchAtLogin, 0, "login waits for the install to finish")
        XCTAssertEqual(harness.installedCommandLineTool, 0)

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .voice }
        XCTAssertEqual(harness.startedDaemon, 0, "the service owns the daemon after a finished apply")
        XCTAssertEqual(harness.launchAtLogin, 1, "the install ends while the person still watches it")
        XCTAssertEqual(harness.installedCommandLineTool, 1)

        state.voiceName = " Ada "
        state.recordVoice()
        await waitUntil { state.step == .tryOut }
        XCTAssertEqual(harness.enrolledNames, ["Ada"])
        XCTAssertEqual(state.enrolled, "Voice saved for Ada.")

        state.dictationEvent(Self.delivered(words: 2))
        state.next()
        XCTAssertEqual(state.step, .openAria)
        XCTAssertEqual(
            state.installNotice, "The aria command was not installed.", "the last page says what did not land")

        state.shortcutOpenedAria()
        XCTAssertEqual(state.step, .done)
        XCTAssertEqual(harness.launchAtLogin, 1, "ending setup starts nothing more")
    }

    func testApplyEventsDriveTheStepAndTheProgress() async {
        let harness = Harness()
        let state = await walkToApplying(harness)
        var started = Aria_Setup_V1_SetupEvent()
        started.started = .models
        var download = Aria_Setup_V1_SetupEvent()
        download.download.modelID = "parakeet"
        download.download.receivedBytes = 5
        download.download.totalBytes = 10

        harness.onEvent?(started)
        harness.onEvent?(download)
        await waitUntil { state.downloadProgress["parakeet"] != nil }

        XCTAssertEqual(state.applying, .models)
        XCTAssertEqual(state.downloadProgress["parakeet"]?.receivedBytes, 5)
        XCTAssertEqual(
            state.runningDownload, ModelTransfer(received: 5, total: 20),
            "the bar counts the model not started yet at its listed size")

        var service = Aria_Setup_V1_SetupEvent()
        service.started = .service
        harness.onEvent?(service)
        await waitUntil { state.applying == .service }
        XCTAssertNil(state.runningDownload, "the download ends when the models step does")
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
        await waitUntil { state.step == .failed(SetupState.applyFailure) }
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
        await waitUntil { state.step == .failed(SetupState.applyFailure) }
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

        harness.ariaKey = .armed
        await waitUntil { state.step == .voice }
        XCTAssertTrue(state.isHotkeyReady, "the last page teaches the Aria key once it arms")
    }

    func testTheGrantPageFollowsThePermissionTheAriaKeyLacks() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        harness.ariaKey = .needsInputMonitoring
        let state = await walkToApplying(harness)

        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.ariaKeyState == .needsInputMonitoring }
        state.openPrivacySettings()

        XCTAssertEqual(state.step, .grant)
        XCTAssertEqual(harness.openedFor, [.needsInputMonitoring])
    }

    func testVoiceIsSkippedWhenSomeoneIsEnrolledOrTheModelWasSkipped() async {
        let harness = Harness()
        harness.speakers = [EnrolledSpeaker(name: "Ada", role: .owner)]
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
        inspection.defaults.sttEngineID = "parakeet-mlx"
        inspection.defaults.ttsEngineID = "futuristic"
        inspection.models = inspection.models.map { model in
            var model = model
            model.files = .complete
            return model
        }
        inspection.missing = []
        let harness = Harness()
        harness.inspection = inspection
        let state = SetupState(effects: harness.effects, runner: SetupRunner())
        XCTAssertNil(state.missingSummary, "nothing to say before the Mac is looked at")
        state.start()
        await waitUntil { state.step == .welcome }
        XCTAssertNil(state.missingSummary, "a Mac with nothing missing needs no line saying so")
        XCTAssertNil(state.expressTotal, "a Mac with everything in place has nothing to download")
        state.express()
        XCTAssertEqual(state.step, .welcome, "an engine this Mac cannot run blocks Express too")
        state.next()

        XCTAssertEqual(state.selections, inspection.defaults)
        XCTAssertTrue(SetupState.plan(inspection, state.selections).isEmpty)
        XCTAssertEqual(state.downloadTotal, "No downloads needed.")
        XCTAssertEqual(state.blockedBy, "Parakeet-Mlx: needs MLX")
        state.next()
        XCTAssertEqual(state.step, .choices, "an engine this Mac cannot run does not pass")
        XCTAssertFalse(SetupState.shouldOffer(inspection, offeredBefore: false))
    }

    func testPlanFollowsTheChosenEngines() {
        let inspection = Self.freshMac()
        var selections = inspection.defaults
        selections.ttsEngineID = "kokoro"

        XCTAssertEqual(
            SetupState.plan(inspection, selections).map(\.id),
            ["parakeet", "kokoro", "speaker", "segmentation"]
        )
    }

    func testPlanLeavesTheLocalEngineAloneWhileTheServerRecognizes() {
        var inspection = Self.freshMac()
        inspection.sttRunsOn = .server

        XCTAssertFalse(SetupState.plan(inspection, inspection.defaults).map(\.id).contains("parakeet"))
    }

    func testPlanSkipsAModelConfiguredElsewhere() {
        var inspection = Self.freshMac()
        inspection.models[0].files = .external

        XCTAssertFalse(SetupState.plan(inspection, inspection.defaults).map(\.id).contains("parakeet"))
    }

    func testExpressPreviewMatchesTheDownloadRequest() async {
        var inspection = Self.freshMac()
        inspection.models[3].files = .complete
        let harness = Harness()
        harness.inspection = inspection
        let state = await welcome(harness)
        let preview = SetupState.expressPlan(state.inspection, state.selections)

        XCTAssertEqual(preview.map(\.id), ["parakeet"], "the installed and the optional are left out")
        XCTAssertEqual(SetupState.total(preview), "Total download: 10 bytes")
        state.express()
        await waitUntil { harness.applyContinuation != nil }
        XCTAssertEqual(harness.applied?.downloads, preview.map(\.id))
    }

    func testTotalNeverClaimsAnExactSizeForAResumedDownload() {
        var resumed = Self.model("parakeet", engine: "parakeet")
        resumed.files = .partial

        XCTAssertEqual(SetupState.total([]), "No downloads needed.")
        XCTAssertEqual(SetupState.total([Self.model("speaker")]), "Total download: 10 bytes")
        XCTAssertEqual(SetupState.total([resumed, Self.model("speaker")]), "Total download: up to 20 bytes")
    }

    func testEngineDownloadTellsWhatPickingItFetches() {
        var inspection = Self.freshMac()
        inspection.models[1].files = .partial
        var nemotron = Self.model("nemotron", engine: "nemotron")
        nemotron.files = .external
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
        XCTAssertEqual(source(.unspecified), .status("Size unknown"), "never shown as nothing to download")
    }

    func testSelfFetchingEngineIsNotedOutsideTheTotal() async {
        var inspection = Self.freshMac()
        inspection.sttChoices[1].availability = .missing
        inspection.sttChoices[1].modelSource = .fetchedOnFirstUse
        inspection.defaults.sttEngineID = "parakeet-mlx"
        let harness = Harness()
        harness.inspection = inspection
        let state = await welcome(harness)

        XCTAssertEqual(state.selfFetchingNotes, ["Parakeet-Mlx downloads its model on first use."])
        XCTAssertEqual(SetupState.expressPlan(state.inspection, state.selections).map(\.id), ["segmentation"])
    }

    func testWizardIsOfferedOnce() {
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .grant
        var inspection = Aria_Setup_V1_Inspection()
        XCTAssertFalse(SetupState.shouldOffer(inspection, offeredBefore: false))
        inspection.missing = [missing]
        XCTAssertTrue(SetupState.shouldOffer(inspection, offeredBefore: false))
        XCTAssertFalse(SetupState.shouldOffer(inspection, offeredBefore: true))
    }

    func testARemappedKeyAloneDoesNotOpenTheWizard() {
        var inspection = Aria_Setup_V1_Inspection()
        var remapped = Aria_Setup_V1_Missing()
        remapped.kind = .keyRemapped
        inspection.missing = [remapped]
        XCTAssertFalse(SetupState.shouldOffer(inspection, offeredBefore: false))
    }

    func testAMissingSttModelReopensTheWizard() throws {
        var inspection = SetupStateTests.freshMac()
        let model = try XCTUnwrap(
            inspection.models.first { $0.engineID == inspection.defaults.sttEngineID })
        var missing = Aria_Setup_V1_Missing()
        missing.kind = .model
        missing.modelID = model.id
        inspection.missing = [missing]

        XCTAssertTrue(SetupState.shouldOffer(inspection, offeredBefore: true))

        missing.modelID = "another-engine"
        inspection.missing = [missing]
        XCTAssertFalse(SetupState.shouldOffer(inspection, offeredBefore: true))
    }

    func testAFailedEnrollmentShowsTheSpeakerFailure() async {
        let harness = Harness()
        harness.enrollError = SpeakerFailure(errorDescription: "That name is already in use.")
        let state = await walkToVoice(harness)

        state.voiceName = "Ada"
        state.recordVoice()
        await waitUntil { !state.isBusy }

        XCTAssertEqual(state.notice, "That name is already in use.")
        XCTAssertEqual(state.step, .voice)
    }

    private static func delivered(
        words: UInt32, outcome: Aria_Daemon_V1_DeliveryOutcome = .inserted
    ) -> Aria_Daemon_V1_DictationEvent {
        var event = Aria_Daemon_V1_DictationEvent()
        event.stage = .delivered
        event.outcome = outcome
        event.wordCount = words
        return event
    }

    func testOnlyAnAriaKeyDictationThatTypedWordsCountsAsATry() async {
        let harness = Harness()
        let state = await walkToVoice(harness)
        state.dictationEvent(Self.delivered(words: 2))
        XCTAssertNil(state.typedWordCount, "a dictation before Try It is no try")
        state.next()

        state.dictationEvent(Self.delivered(words: 0))
        state.dictationEvent(Self.delivered(words: 2, outcome: .copied))
        XCTAssertNil(state.typedWordCount, "a dictation that typed nothing is no try")

        state.dictationEvent(Self.delivered(words: 13))
        XCTAssertEqual(state.typedWordCount, 13)
        XCTAssertEqual(state.step, .tryOut, "the words stay in view until the person moves on")
        state.next()
        XCTAssertEqual(state.step, .openAria)
    }

    func testOnlyOpeningAriaDuringItsStepFinishesSetup() async {
        let harness = Harness()
        let state = await walkToVoice(harness)
        state.shortcutOpenedAria()
        XCTAssertEqual(state.step, .voice, "opening Aria before its step is no try")
        state.next()
        state.shortcutOpenedAria()
        XCTAssertEqual(state.step, .tryOut)

        state.next()
        XCTAssertEqual(state.step, .openAria, "skipping Try It still teaches Open Aria")
        state.shortcutOpenedAria()
        XCTAssertEqual(state.step, .done)
    }

    func testAccessGrantedAfterSkipReachesTryIt() async {
        let harness = Harness()
        harness.summary = Self.summary(grant: .waiting)
        let state = await walkToApplying(harness)
        harness.applyContinuation?.resume(returning: harness.summary)
        await waitUntil { state.step == .grant }
        state.next()
        await waitUntil { state.step == .voice }
        state.next()
        XCTAssertFalse(state.isHotkeyReady)

        harness.ariaKey = .armed
        await waitUntil { state.isHotkeyReady }
        XCTAssertEqual(state.step, .tryOut)
    }

    func testClosingTheLastPageEndsSetup() async {
        let harness = Harness()
        let state = await walkToVoice(harness)
        state.next()
        state.next()
        XCTAssertEqual(state.step, .openAria)

        state.stop()
        XCTAssertEqual(state.step, .done)
        state.start()
        XCTAssertEqual(state.step, .inspecting, "reopening starts a new walk")
    }

    func testDoneEndsSetupWithoutTheShortcut() async {
        let harness = Harness()
        let state = await walkToVoice(harness)
        state.next()
        state.next()
        XCTAssertEqual(state.step, .openAria)
        state.next()
        XCTAssertEqual(state.step, .done)
    }

    func testASecondWalkStartsWithoutTheFirstOnesResults() async {
        let harness = Harness()
        let state = await walkToVoice(harness)
        state.voiceName = "Ada"
        state.recordVoice()
        await waitUntil { state.step == .tryOut }
        state.typed = "hello there"
        state.dictationEvent(Self.delivered(words: 2))
        state.next()
        state.next()
        XCTAssertEqual(state.step, .done)

        state.start()

        XCTAssertEqual(state.typed, "")
        XCTAssertNil(state.typedWordCount)
        XCTAssertNil(state.enrolled)
        XCTAssertEqual(state.voiceName, "")
    }
}
