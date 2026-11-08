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
        controller.checkForUpdates(nil)
    }
}

// Sparkle calls its user driver's delegate on the main thread.
private final class ReminderDriverDelegate: NSObject, SPUStandardUserDriverDelegate {
    var setAvailableVersion: @MainActor (String?) -> Void = { _ in }

    var supportsGentleScheduledUpdateReminders: Bool { true }

    // A routine update waits behind the menu's update item.
    // A critical one interrupts, but only near launch, where Sparkle brings it to the front.
    func standardUserDriverShouldHandleShowingScheduledUpdate(
        _ update: SUAppcastItem,
        andInImmediateFocus immediateFocus: Bool
    ) -> Bool {
        update.isCriticalUpdate && immediateFocus
    }

    func standardUserDriverWillHandleShowingUpdate(
        _ handleShowingUpdate: Bool,
        forUpdate update: SUAppcastItem,
        state: SPUUserUpdateState
    ) {
        guard !handleShowingUpdate else { return }
        publishAvailableVersion(update.displayVersionString)
    }

    // The menu's update item is the only way back to a hidden update, so it stays until the session ends.
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
