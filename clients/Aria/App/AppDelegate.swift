import AppKit
import Combine
import SwiftUI
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "AppDelegate")

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let settings = WindowPresentation()
    let setup = WindowPresentation()
    let setupState = SetupState(effects: .live)
    let setupStatus = SetupStatus()
    let voices: VoiceProfiles
    lazy var serverTranscription = ServerTranscription(effects: .live, setup: setupStatus)
    let updates = UpdateController()

    static let captionsEnabledKey = "captionsEnabled"
    static let areCaptionsEnabledByDefault = false
    private static let setupOfferedKey = "setupOffered"

    private let events = DaemonEvents()
    private let panel = FloatingPanel(
        width: OverlayStyle.captionWidth, bottomMargin: OverlayStyle.captionBottomMargin)
    let status: StatusController
    private let ghost: GhostController
    let askShortcut: PressController
    let promptHotkeys: PromptHotkeys
    let promptList: PromptList
    private var caption = CaptionState()
    private var shownCommitted: String?
    private var shownProvisional: String?
    private var expiryTimer: Timer?
    private var daemonSettings: DaemonSettings?
    private var askChordChange: AnyCancellable?
    lazy var settingsHealth = SettingsHealth(
        effects: .live(reloadSettings: { [weak self] in await self?.daemonSettings?.sync() }))

    override init() {
        promptHotkeys = PromptHotkeys(events: events)
        promptList = PromptList(prompts: promptHotkeys)
        voices = VoiceProfiles(effects: .live)
        let status = StatusController(events: events, prompts: promptHotkeys)
        self.status = status
        ghost = GhostController(events: events)
        askShortcut = PressController(events: events, operation: .ask) { [weak status] run in
            status?.noteAsk(run)
        }
        super.init()
        events.onHotkey { [weak self] combo in
            self?.run(combo)
        }
    }

    /// Before the first snapshot no action holds a combo, so nothing runs.
    private func run(_ combo: Aria_Daemon_V1_ShortcutCombo) {
        guard let snapshot = daemonSettings?.snapshot else { return }
        switch ComboAction(combo: combo, in: snapshot) {
        case .ask: askShortcut.press()
        case .promptList: promptList.press()
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
        promptList.track(settings)
    }

    func applicationWillTerminate(_ notification: Notification) {
        do {
            try DaemonClient.signalStop()
        } catch {
            log.error("Daemon not stopped on quit: \(String(describing: error), privacy: .private)")
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let launchKind = NSAppleEventManager.shared().currentAppleEvent?
            .paramDescriptor(forKeyword: keyAEPropData)?.enumCodeValue
        settings.didLaunch(atLogin: launchKind == keyAELaunchedAsLogInItem)
        UserDefaults.standard.register(defaults: [Self.captionsEnabledKey: Self.areCaptionsEnabledByDefault])

        caption.setEnabled(UserDefaults.standard.bool(forKey: Self.captionsEnabledKey))
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
                self.caption.setEnabled(UserDefaults.standard.bool(forKey: Self.captionsEnabledKey))
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
        offerSetupOnce()
    }

    /// A Mac with something missing gets the setup wizard once; after that,
    /// setup waits in Settings → General.
    private func offerSetupOnce() {
        let defaults = UserDefaults.standard
        Task { [weak self] in
            do {
                let inspection = try await SetupClient.inspect()
                self?.setupStatus.show(inspection)
                let offeredBefore = defaults.bool(forKey: Self.setupOfferedKey)
                guard SetupState.shouldOffer(missing: inspection.missing, offeredBefore: offeredBefore) else { return }
                defaults.set(true, forKey: Self.setupOfferedKey)
                self?.setup.request()
            } catch {
                log.error("Setup inspection failed: \(String(describing: error), privacy: .private)")
            }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        settings.request()
        return false
    }

    private func render() {
        guard caption.committed != shownCommitted || caption.provisional != shownProvisional else {
            return
        }
        shownCommitted = caption.committed
        shownProvisional = caption.provisional
        if caption.committed.isEmpty && caption.provisional.isEmpty {
            panel.hide()
        } else {
            panel.show(CaptionView(committed: caption.committed, provisional: caption.provisional))
        }
    }
}
