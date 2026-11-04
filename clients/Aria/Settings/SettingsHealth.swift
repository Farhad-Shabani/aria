import AppKit
import Foundation
import UniformTypeIdentifiers
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SettingsHealth")

/// One notice about changing settings. An invalid file is a setup item instead.
enum SettingsAttention: Equatable, Hashable {
    case readOnly
    case unsaved
    case restartNeeded
    case reverted

    /// Problems first.
    static func all(
        status: Aria_Daemon_V1_SettingsStatus?,
        isReadOnly: Bool,
        hasUnsavedChange: Bool = false
    ) -> [SettingsAttention] {
        var items: [SettingsAttention] = []
        if isReadOnly { items.append(.readOnly) }
        if hasUnsavedChange { items.append(.unsaved) }
        if let status {
            if !status.restartRequired.isEmpty { items.append(.restartNeeded) }
            if !status.reverted.isEmpty { items.append(.reverted) }
        }
        return items
    }

    var message: String {
        switch self {
        case .readOnly:
            return
                "Aria can’t save settings because your settings file is read-only. Change it where it’s managed."
        case .unsaved:
            return "Aria hasn’t saved your last change yet. It tries again the next time it reads your settings."
        case .restartNeeded:
            return "Some settings apply after a restart."
        case .reverted:
            return "A save in another app undid a change you made in Aria."
        }
    }

    var isProblem: Bool {
        switch self {
        case .readOnly, .unsaved: true
        case .restartNeeded, .reverted: false
        }
    }
}

/// Follows the daemon's settings status.
@MainActor
final class SettingsHealth: ObservableObject {
    struct Effects {
        var status: () async throws -> Aria_Daemon_V1_SettingsStatus
        /// Reads the values again: a status change may mean a hand edit.
        var reloadSettings: () async -> Void
        var settingsFile: SettingsFile.Effects
    }

    /// Nil while no daemon reports.
    @Published private(set) var status: Aria_Daemon_V1_SettingsStatus?
    @Published private(set) var openFailure: String?

    private let effects: Effects

    init(effects: Effects) {
        self.effects = effects
    }

    /// From the event stream, which sends nil whenever it carries nothing.
    func receive(_ status: Aria_Daemon_V1_SettingsStatus?) async {
        self.status = status
        if status != nil { await effects.reloadSettings() }
    }

    /// A lost stream cleared the status, and a fault that hasn't changed won't
    /// arrive again.
    func streamOpened() async {
        await refresh()
        await effects.reloadSettings()
    }

    /// The stream doesn't replay, so a daemon that just came up is asked.
    func refresh() async {
        do {
            status = try await effects.status()
        } catch {
            log.notice("Settings status unavailable: \(String(describing: error), privacy: .private)")
            status = nil
        }
    }

    func openSettingsFile() async {
        openFailure = await SettingsFile.open(effects.settingsFile)
    }
}

@MainActor
enum SettingsFile {
    struct Effects {
        var path: () async throws -> URL
        var exists: (URL) -> Bool
        /// Through the CLI, which writes the header and takes the lock.
        var create: () async throws -> Void
        var openInTextEditor: (URL) async throws -> Void
    }

    /// Nil when the file opened; otherwise what to tell the person.
    static func open(_ effects: Effects) async -> String? {
        do {
            let path = try await effects.path()
            if !effects.exists(path) { try await effects.create() }
            try await effects.openInTextEditor(path)
            return nil
        } catch {
            log.error("Opening the settings file failed: \(String(describing: error), privacy: .private)")
            return "Aria couldn’t open your settings file. Try again."
        }
    }
}

extension SettingsFile.Effects {
    @MainActor static var live: Self {
        .init(
            path: { try await DaemonProcess.settingsPath() },
            exists: { FileManager.default.fileExists(atPath: $0.path) },
            create: { try await DaemonProcess.createSettingsFile() },
            openInTextEditor: { url in
                // A .toml has no default app of its own, so ask for the plain text one.
                guard let editor = NSWorkspace.shared.urlForApplication(toOpen: .plainText) else {
                    throw DaemonProcessError.commandFailed("no app opens plain text")
                }
                _ = try await NSWorkspace.shared.open(
                    [url], withApplicationAt: editor, configuration: NSWorkspace.OpenConfiguration())
            }
        )
    }
}

extension SettingsHealth.Effects {
    @MainActor static func live(reloadSettings: @escaping () async -> Void) -> Self {
        .init(
            status: { try await SettingsClient().status() },
            reloadSettings: reloadSettings,
            settingsFile: .live
        )
    }
}
