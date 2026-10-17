import AppKit
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let settings = WindowPresentation()
    let setup = WindowPresentation()
    let setupState = SetupState(effects: .live)
    let updates = UpdateController()

    static let enabledKey = "captionsEnabled"
    static let setupOfferedKey = "setupOffered"
    // Matches the daemon's delivery_mode default: streaming, captions off.
    static let enabledByDefault = false

    private let events = DaemonEvents()
    private let panel = FloatingPanel(width: captionWidth, bottomMargin: captionBottomMargin)
    let status: StatusController
    private let askHotkey: AskHotkeyController
    private var caption = CaptionState()
    private var shownCommitted: String?
    private var shownProvisional: String?
    private var expiryTimer: Timer?

    override init() {
        status = StatusController(events: events)
        askHotkey = AskHotkeyController(events: events)
        super.init()
    }

    /// The chord follows the daemon's stored preference.
    func trackAskHotkey(_ preferences: DaemonPreferences) {
        askHotkey.track(preferences)
    }

    /// The app closed means no daemon runs: whichever daemon holds the lock,
    /// the service's or a spawned one, gets the same SIGTERM as the toggle.
    func applicationWillTerminate(_ notification: Notification) {
        do {
            try DaemonClient.signalStop()
        } catch {
            NSLog("AppDelegate: daemon not stopped on quit — \(error)")
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let launchKind = NSAppleEventManager.shared().currentAppleEvent?
            .paramDescriptor(forKeyword: keyAEPropData)?.enumCodeValue
        settings.didLaunch(atLogin: launchKind == keyAELaunchedAsLogInItem)
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.enabledByDefault])

        caption.setEnabled(UserDefaults.standard.bool(forKey: Self.enabledKey))
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
        // The menu's toggle writes to UserDefaults; react without waiting for
        // the next caption. The notification can arrive on any thread.
        NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.caption.setEnabled(UserDefaults.standard.bool(forKey: Self.enabledKey))
                self.render()
            }
        }

        events.onSpeaker { [weak self] event in
            if let event { self?.setupState.speakerEvent(event) }
        }

        status.start()
        events.start()
        offerSetupOnce()
    }

    /// A Mac with something missing gets the wizard once; after that it
    /// waits in the menu.
    private func offerSetupOnce() {
        let defaults = UserDefaults.standard
        Task { [weak self] in
            do {
                let inspection = try await SetupClient.inspect()
                let offeredBefore = defaults.bool(forKey: Self.setupOfferedKey)
                guard SetupState.shouldOffer(missing: inspection.missing, offeredBefore: offeredBefore) else { return }
                defaults.set(true, forKey: Self.setupOfferedKey)
                self?.setup.request()
            } catch {
                NSLog("AppDelegate: setup inspection failed — \(error)")
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
