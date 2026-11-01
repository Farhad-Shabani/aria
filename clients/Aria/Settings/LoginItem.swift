import ServiceManagement
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "LoginItem")

/// macOS owns the login item status, so each change reads it back, even after
/// a failed register or unregister.
@MainActor
final class LoginItem: ObservableObject {
    @Published private(set) var isEnabled = SMAppService.mainApp.status == .enabled

    func refresh() {
        isEnabled = SMAppService.mainApp.status == .enabled
    }

    func set(enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            log.error("Launch at Login toggle failed: \(String(describing: error), privacy: .private)")
        }
        refresh()
    }
}
