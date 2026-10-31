import AppKit
import Combine
import Sparkle
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "UpdateController")

@MainActor
final class UpdateController: ObservableObject {
    // Sparkle's update alert changes these too, so they only follow its KVO.
    @Published private(set) var canCheck = false
    @Published private(set) var checksAutomatically = false
    @Published private(set) var installsAutomatically = false
    @Published private(set) var availableVersion: String?

    private let updaterDelegate = AbortLoggingDelegate()
    private let driverDelegate = ReminderDriverDelegate()
    private let controller: SPUStandardUpdaterController

    init() {
        // Debug builds are version 0.0.0 and would replace themselves.
        #if DEBUG
            let starting = false
        #else
            let starting = true
        #endif
        controller = SPUStandardUpdaterController(
            startingUpdater: starting,
            updaterDelegate: updaterDelegate,
            userDriverDelegate: driverDelegate
        )
        controller.updater.publisher(for: \.canCheckForUpdates).assign(to: &$canCheck)
        controller.updater.publisher(for: \.automaticallyChecksForUpdates).assign(to: &$checksAutomatically)
        controller.updater.publisher(for: \.automaticallyDownloadsUpdates).assign(to: &$installsAutomatically)
        driverDelegate.setAvailableVersion = { [weak self] version in self?.availableVersion = version }
    }

    var isUpdateAvailable: Bool { availableVersion != nil }

    var actionTitle: String {
        availableVersion.map { "Update to \($0)…" } ?? "Check for Updates…"
    }

    func setChecksAutomatically(_ on: Bool) {
        controller.updater.automaticallyChecksForUpdates = on
    }

    func setInstallsAutomatically(_ on: Bool) {
        controller.updater.automaticallyDownloadsUpdates = on
    }

    func checkForUpdates() {
        NSApplication.shared.activate()
        controller.checkForUpdates(nil)
    }
}

// Sparkle calls its user driver's delegate on the main thread.
private final class ReminderDriverDelegate: NSObject, SPUStandardUserDriverDelegate {
    var setAvailableVersion: @MainActor (String?) -> Void = { _ in }

    var supportsGentleScheduledUpdateReminders: Bool { true }

    // Sparkle asks for focus only just after launch.
    // Otherwise it puts its alert behind other apps, where nobody sees it.
    func standardUserDriverShouldHandleShowingScheduledUpdate(
        _ update: SUAppcastItem,
        andInImmediateFocus immediateFocus: Bool
    ) -> Bool {
        immediateFocus
    }

    func standardUserDriverWillHandleShowingUpdate(
        _ handleShowingUpdate: Bool,
        forUpdate update: SUAppcastItem,
        state: SPUUserUpdateState
    ) {
        if handleShowingUpdate {
            MainActor.assumeIsolated { NSApplication.shared.activate() }
        } else {
            publishAvailableVersion(update.displayVersionString)
        }
    }

    // Sparkle's alert can fall behind other apps, so the reminder stays until the session ends.
    func standardUserDriverWillFinishUpdateSession() {
        publishAvailableVersion(nil)
    }

    private func publishAvailableVersion(_ version: String?) {
        let setAvailableVersion = setAvailableVersion
        MainActor.assumeIsolated { setAvailableVersion(version) }
    }
}

private final class AbortLoggingDelegate: NSObject, SPUUpdaterDelegate {
    func updater(_ updater: SPUUpdater, didAbortWithError error: any Error) {
        let error = error as NSError
        // Sparkle reports a check that found nothing as an abort too.
        let foundNothing = error.domain == SUSparkleErrorDomain && error.code == Int(SUError.noUpdateError.rawValue)
        guard !foundNothing else { return }
        log.error(
            "Update aborted: code \(error.code, privacy: .public), \(error.localizedDescription, privacy: .private)"
        )
    }
}
