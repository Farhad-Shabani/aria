import Foundation

enum CommandLineToolError: LocalizedError {
    case notInApplications(String)
    case occupied(String)
    case failed(String)

    var errorDescription: String? {
        switch self {
        case .notInApplications:
            return
                "The `aria` command needs Aria in your Applications folder. Move Aria there, then open Aria Setup again."
        case .occupied(let path):
            return "\(path) already exists and isn’t Aria’s. Remove it, then open Aria Setup again."
        case .failed: return CommandLineTool.notInstalled
        }
    }
}

/// The command links into the bundle so it follows the app through updates.
enum CommandLineTool {
    static let notInstalled =
        "The `aria` command isn’t installed. To add it, open Aria Setup in Settings > General."

    private static let link = URL(fileURLWithPath: "/usr/local/bin/aria")
    private static let bundledCommand = "Contents/Helpers/aria"
    private static let userCancelled = -128

    private static var target: URL { Bundle.main.bundleURL.appending(path: bundledCommand) }

    private static var isInstalled: Bool { linkDestination() == target.path }

    private static var linksIntoAria: Bool { linkDestination()?.hasSuffix(".app/" + bundledCommand) ?? false }

    /// A build folder or the translocated copy macOS runs from a DMG goes
    /// away, and a link into it would die with it.
    static func isInstalledLocation(_ bundle: URL) -> Bool {
        bundle.pathComponents.contains("Applications") && !bundle.path.contains("/AppTranslocation/")
    }

    /// Returns false when the user cancels the password dialog.
    @MainActor
    static func install() throws -> Bool {
        if isInstalled { return true }
        guard isInstalledLocation(Bundle.main.bundleURL) else {
            throw CommandLineToolError.notInApplications(Bundle.main.bundleURL.path)
        }
        // fileExists follows links, so a dangling one would pass as absent.
        let occupied = linkDestination() != nil || FileManager.default.fileExists(atPath: link.path)
        if occupied && !linksIntoAria {
            throw CommandLineToolError.occupied(link.path)
        }
        let command = "mkdir -p /usr/local/bin && ln -sfn \(shellQuoted(target.path)) \(shellQuoted(link.path))"
        return try runAsAdministrator(command, prompt: "Aria installs the aria command in /usr/local/bin.")
    }

    private static func linkDestination() -> String? {
        try? FileManager.default.destinationOfSymbolicLink(atPath: link.path)
    }

    /// Runs in this process, so the password dialog names Aria, not osascript.
    @MainActor
    private static func runAsAdministrator(_ command: String, prompt: String) throws -> Bool {
        let source =
            "do shell script \(appleScriptQuoted(command)) with prompt \(appleScriptQuoted(prompt)) with administrator privileges"
        guard let script = NSAppleScript(source: source) else {
            throw CommandLineToolError.failed("could not build the install script")
        }
        var errorInfo: NSDictionary?
        script.executeAndReturnError(&errorInfo)
        guard let errorInfo else { return true }
        if errorInfo[NSAppleScript.errorNumber] as? Int == userCancelled { return false }
        let message = errorInfo[NSAppleScript.errorMessage] as? String ?? "the install script failed"
        throw CommandLineToolError.failed(message)
    }

    static func shellQuoted(_ text: String) -> String {
        "'" + text.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    static func appleScriptQuoted(_ text: String) -> String {
        let escaped =
            text
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        return "\"" + escaped + "\""
    }
}
