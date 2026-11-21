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

    /// Dropping the Dock icon hands the keys to another app, so it waits
    /// while one of Aria's panels holds them.
    static func policy(
        windows: [NSWindow], keyWindow: NSWindow?, current: NSApplication.ActivationPolicy
    ) -> NSApplication.ActivationPolicy {
        if needsDockIcon(windows: windows) { return .regular }
        return keyWindow is NSPanel ? current : .accessory
    }

    @objc private func update() {
        let app = NSApplication.shared
        // Hiding the app makes its windows read as not visible.
        guard !app.isHidden else { return }
        let policy = Self.policy(windows: app.windows, keyWindow: app.keyWindow, current: app.activationPolicy())
        guard policy != app.activationPolicy() else { return }
        app.setActivationPolicy(policy)
        if policy == .regular { app.activate() }
    }
}
