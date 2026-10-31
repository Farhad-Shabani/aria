import AppKit

/// macOS shows an app's top menu bar only for a regular app, which also gets a
/// Dock icon. Aria is regular only while one of its windows is open.
@MainActor
final class DockPresence: NSObject {
    func start() {
        // A window opened while another app is active posts no key change.
        NotificationCenter.default.addObserver(
            self, selector: #selector(update), name: NSApplication.didUpdateNotification, object: nil)
    }

    /// Panels are the overlays and the menu bar extra, which never need a Dock icon.
    static func needsDockIcon(windows: [NSWindow]) -> Bool {
        windows.contains { window in
            !(window is NSPanel)
                && window.styleMask.contains(.titled)
                && (window.isVisible || window.isMiniaturized)
        }
    }

    @objc private func update() {
        let app = NSApplication.shared
        // Hiding the app makes its windows read as not visible.
        guard !app.isHidden else { return }
        let policy: NSApplication.ActivationPolicy = Self.needsDockIcon(windows: app.windows) ? .regular : .accessory
        guard policy != app.activationPolicy() else { return }
        app.setActivationPolicy(policy)
        if policy == .regular { app.activate() }
    }
}
