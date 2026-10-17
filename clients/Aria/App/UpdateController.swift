import AppKit
import Combine
import Sparkle

@MainActor
final class UpdateController: ObservableObject {
    // Sparkle's update alert changes these too, so they only follow its KVO.
    @Published private(set) var canCheck = false
    @Published private(set) var checksAutomatically = false
    @Published private(set) var installsAutomatically = false

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
            updaterDelegate: nil,
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
