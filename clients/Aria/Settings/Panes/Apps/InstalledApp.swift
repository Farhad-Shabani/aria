import AppKit
import UniformTypeIdentifiers

/// How an app looks in Settings. An app that is no longer installed shows
/// its bundle ID and a generic icon.
struct InstalledApp {
    let name: String
    let icon: NSImage

    init(bundleID: String) {
        let workspace = NSWorkspace.shared
        guard let url = workspace.urlForApplication(withBundleIdentifier: bundleID) else {
            name = bundleID
            icon = workspace.icon(for: .application)
            return
        }
        let displayName = FileManager.default.displayName(atPath: url.path)
        name = displayName.hasSuffix(".app") ? String(displayName.dropLast(".app".count)) : displayName
        icon = workspace.icon(forFile: url.path)
    }

    /// The lookup reads the disk, and Settings redraws its app rows on every
    /// preference change. An app installed while Aria runs keeps showing its
    /// bundle ID until Aria relaunches.
    @MainActor
    static func lookUp(_ bundleID: String) -> InstalledApp {
        if let app = lookedUp[bundleID] { return app }
        let app = InstalledApp(bundleID: bundleID)
        lookedUp[bundleID] = app
        return app
    }

    @MainActor private static var lookedUp: [String: InstalledApp] = [:]

    /// Nil when the user cancels. An app without a bundle ID cannot be told
    /// apart when in front, so choosing one beeps and adds nothing.
    @MainActor
    static func choose() -> String? {
        let panel = NSOpenPanel()
        panel.directoryURL = URL(filePath: "/Applications")
        panel.allowedContentTypes = [.application]
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        guard let bundleID = Bundle(url: url)?.bundleIdentifier else {
            NSSound.beep()
            return nil
        }
        return bundleID
    }
}
