import AppKit
import Combine
import SwiftUI
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "AppDelegate")

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let settings = WindowPresentation()
    let settingsPaneRequest = SettingsPaneRequest()
    let setup = WindowPresentation()
    let setupRunner = SetupRunner()
    lazy var setupState = SetupState(effects: .live, runner: setupRunner)
    let setupStatus = SetupStatus()
    lazy var modelDownload = ModelDownload(effects: .live, runner: setupRunner)
    lazy var sttEngineChange = SttEngineChange(effects: .live, runner: setupRunner)
    let voices: VoiceProfiles
    lazy var serverStt = ServerStt(effects: .live, setup: setupStatus, runner: setupRunner)
    let updates = UpdateController()
    let history = HistoryState(effects: HistoryClient.effects)
    let chats = Chats(effects: .live)

    private static let setupOfferedKey = "setupOffered"

    private let dockPresence = DockPresence()
    private let events = DaemonEvents()
    private let panel = FloatingPanel(
        width: OverlayStyle.captionWidth, bottomMargin: OverlayStyle.captionBottomMargin)
    let status: StatusController
    let systemTranscriptionStatus: SystemTranscriptionStatus
    private let ghost: GhostController
    /// Lazy, since it opens the palette on the chat it asks into, and the palette takes it
    /// in turn.
    private(set) lazy var askShortcut: PressController = PressController(
        events: events,
        operation: .ask(
            chats, target: { [weak self] in self?.palette.askTarget },
            show: { [weak self] in self?.palette.showChat() })
    ) { [weak self] run in
        self?.status.noteAsk(run)
    }
    let promptHotkeys: PromptHotkeys
    private var caption = CaptionState()
    private var shownCommitted: String?
    private var shownProvisional: String?
    private var expiryTimer: Timer?
    private var daemonSettings: DaemonSettings?
    private var askChordChange: AnyCancellable?
    private(set) lazy var palette = PaletteController(
        prompts: promptHotkeys, ask: askShortcut, systemTranscription: systemTranscriptionStatus, history: history,
        chats: chats, events: events,
        setup: setupStatus, settingsHealth: settingsHealth,
        openSettings: { [weak self] pane in
            guard let self else { return }
            if let pane { settingsPaneRequest.pane = pane }
            settings.request()
        },
        openSetup: { [weak self] in self?.setup.request() },
        delivered: { [weak status] outcome in status?.noteDelivery(outcome) })
    lazy var settingsHealth = SettingsHealth(
        effects: .live(reloadSettings: { [weak self] in await self?.daemonSettings?.sync() }))

    override init() {
        promptHotkeys = PromptHotkeys(events: events)
        voices = VoiceProfiles(effects: .live)
        let status = StatusController(events: events, prompts: promptHotkeys)
        self.status = status
        systemTranscriptionStatus = SystemTranscriptionStatus(events: events)
        ghost = GhostController(events: events)
        super.init()
        events.onChat { [weak chats] event in chats?.apply(event) }
        events.onStreamOpened { [weak chats] in chats?.reconnected() }
        status.skips = { [weak self] ticket in self?.palette.shows(ticket: ticket) ?? false }
        events.onHotkey { [weak self] combo in
            self?.run(combo)
        }
        events.onDictation { [weak self] event in
            guard let event, [.delivered, .failed, .cancelled].contains(event.stage) else { return }
            self?.history.dictationEnded()
        }
    }

    /// Before the first snapshot no action holds a combo, so nothing runs.
    private func run(_ combo: ShortcutCombo) {
        guard let snapshot = daemonSettings?.snapshot else { return }
        switch ComboAction(combo: combo, in: snapshot) {
        case .ask: askShortcut.press()
        case .palette: palette.press()
        case nil: break
        }
    }

    /// A menu bar scene body sits idle while the menu is closed, so the
    /// stored chords are watched rather than read through a SwiftUI
    /// `onChange`.
    func trackHotkeys(_ settings: DaemonSettings) {
        self.daemonSettings = settings
        events.onSettings { [weak self] status in
            guard let health = self?.settingsHealth else { return }
            Task { await health.receive(status) }
        }
        events.onStreamOpened { [weak self] in
            guard let health = self?.settingsHealth else { return }
            Task { await health.streamOpened() }
        }
        askChordChange = settings.$snapshot
            .map { $0?.askShortcut.storedChord }
            .sink { [weak self] chord in
                Task { @MainActor in self?.askShortcut.chord.bind(chord) }
            }
        promptHotkeys.track(settings)
        palette.track(settings)
    }

    func applicationWillTerminate(_ notification: Notification) {
        do {
            try DaemonProcess.signalStop()
        } catch {
            log.error("Daemon not stopped on quit: \(String(describing: error), privacy: .private)")
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        dockPresence.start()
        let launchKind = NSAppleEventManager.shared().currentAppleEvent?
            .paramDescriptor(forKeyword: keyAEPropData)?.enumCodeValue
        if launchKind != keyAELaunchedAsLogInItem { palette.open() }
        UserDefaults.standard.register(defaults: [CaptionState.enabledKey: CaptionState.isEnabledByDefault])

        caption.setEnabled(UserDefaults.standard.bool(forKey: CaptionState.enabledKey))
        events.onCaption { [weak self] snapshot in
            guard let self else { return }
            caption.apply(snapshot, now: ProcessInfo.processInfo.systemUptime)
            render()
        }
        let timer = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.caption.expire(now: ProcessInfo.processInfo.systemUptime)
                self.render()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        expiryTimer = timer
        // The Settings toggle writes to UserDefaults; react without waiting
        // for the next caption. The notification can arrive on any thread.
        NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.caption.setEnabled(UserDefaults.standard.bool(forKey: CaptionState.enabledKey))
                self.render()
            }
        }

        events.onSpeaker { [weak self] event in
            guard let self, let event else { return }
            setupState.speakerEvent(event)
            voices.speakerEvent(event)
        }

        status.start()
        ghost.start()
        events.start()
        offerSetup()
    }

    private func offerSetup() {
        let defaults = UserDefaults.standard
        Task { [weak self] in
            do {
                let inspection = try await SetupClient.inspect()
                self?.setupStatus.show(inspection)
                let offeredBefore = defaults.bool(forKey: Self.setupOfferedKey)
                guard SetupState.shouldOffer(inspection, offeredBefore: offeredBefore) else { return }
                defaults.set(true, forKey: Self.setupOfferedKey)
                self?.setup.request()
            } catch {
                log.error("Setup inspection failed: \(String(describing: error), privacy: .private)")
            }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        // AppKit restores a minimized window only when this returns true.
        if DockPresence.needsDockIcon(windows: sender.windows) { return true }
        palette.open()
        return false
    }

    private func render() {
        guard caption.finalized != shownCommitted || caption.provisional != shownProvisional else {
            return
        }
        shownCommitted = caption.finalized
        shownProvisional = caption.provisional
        if caption.finalized.isEmpty && caption.provisional.isEmpty {
            panel.hide()
        } else {
            panel.show(CaptionView(finalized: caption.finalized, provisional: caption.provisional))
        }
    }
}
