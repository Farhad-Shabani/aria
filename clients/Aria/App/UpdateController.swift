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

    private let updaterDelegate = AbortLoggingDelegate()
    private let driverDelegate = ForegroundDriverDelegate()
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

// Without a Dock icon, Sparkle's window opens behind the front app.
private final class ForegroundDriverDelegate: NSObject, SPUStandardUserDriverDelegate {
    var supportsGentleScheduledUpdateReminders: Bool { true }

    func standardUserDriverWillHandleShowingUpdate(
        _ handleShowingUpdate: Bool,
        forUpdate update: SUAppcastItem,
        state: SPUUserUpdateState
    ) {
        // Sparkle calls its user driver's delegate on the main thread.
        MainActor.assumeIsolated {
            if handleShowingUpdate { NSApplication.shared.activate() }
        }
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
