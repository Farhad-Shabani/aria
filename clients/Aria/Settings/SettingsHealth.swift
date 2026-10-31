import AppKit
import Foundation
import UniformTypeIdentifiers
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SettingsHealth")

/// What `aria settings check --json` found wrong with the file.
struct InvalidSettings: Equatable, Sendable {
    /// Nil when the fault has no place in the file.
    let line: Int?

    init(line: Int?) {
        self.line = line
    }

    /// Nil for a valid file, or for output this build can't read.
    init?(checkOutput stdout: Data) {
        struct Report: Decodable {
            struct Fault: Decodable { let line: Int? }
            let valid: Bool
            let error: Fault?
        }
        guard let report = try? JSONDecoder().decode(Report.self, from: stdout), !report.valid else {
            return nil
        }
        line = report.error?.line
    }

    /// Never the parser's own text, which can quote the file.
    var mistake: String {
        guard let line else { return "your settings file has a mistake" }
        return "your settings file has a mistake on line \(line)"
    }
}

/// One thing the person should know about the settings file.
enum SettingsAttention: Equatable, Hashable {
    case invalid(line: Int?)
    case startBlocked(line: Int?)
    case readOnly
    case unsaved
    case restartNeeded
    case reverted

    /// Worst first, so a broken file reads before what waits on it.
    static func all(
        status: Aria_Daemon_V1_SettingsStatus?,
        startFailure: DaemonControlFailure?,
        isReadOnly: Bool,
        hasUnsavedChange: Bool = false
    ) -> [SettingsAttention] {
        var items: [SettingsAttention] = []
        if case .settingsInvalid(let invalid) = startFailure {
            items.append(.startBlocked(line: invalid.line))
        }
        if let status, status.hasFault {
            items.append(.invalid(line: status.fault.hasPosition ? Int(status.fault.position.line) : nil))
        }
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
        case .invalid(let line):
            let mistake = InvalidSettings(line: line).mistake
            return "\(mistake.prefix(1).uppercased())\(mistake.dropFirst()). Fix it and save the file."
        case .startBlocked(let line):
            return DaemonControlFailure.settingsInvalid(InvalidSettings(line: line)).message
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
        case .invalid, .startBlocked, .readOnly, .unsaved: true
        case .restartNeeded, .reverted: false
        }
    }

    var opensFile: Bool {
        switch self {
        case .invalid, .startBlocked: true
        case .readOnly, .unsaved, .restartNeeded, .reverted: false
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
            path: { try await DaemonClient.settingsPath() },
            exists: { FileManager.default.fileExists(atPath: $0.path) },
            create: { try await DaemonClient.createSettingsFile() },
            openInTextEditor: { url in
                // A .toml has no default app of its own, so ask for the plain text one.
                guard let editor = NSWorkspace.shared.urlForApplication(toOpen: .plainText) else {
                    throw DaemonClientError.commandFailed("no app opens plain text")
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
            status: { try await UnixSocketSettings().status() },
            reloadSettings: reloadSettings,
            settingsFile: .live
        )
    }
}
