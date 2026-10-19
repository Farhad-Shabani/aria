import Foundation

enum CommandLineToolError: LocalizedError {
    case notInApplications(String)
    case occupied(String)
    case failed(String)

    var errorDescription: String? {
        switch self {
        case .notInApplications(let path): return "Aria runs from \(path); move it to Applications first."
        case .occupied(let path): return "\(path) already exists and is not Aria's. Remove it first."
        case .failed(let message): return message
        }
    }
}

/// The `aria` command, linked into /usr/local/bin, which every shell has on
/// PATH. The link points into the bundle, so it follows the app through
/// updates.
enum CommandLineTool {
    static let link = URL(fileURLWithPath: "/usr/local/bin/aria")
    private static let bundledCommand = "Contents/Helpers/aria"
    /// AppleScript's error number for a cancelled dialog.
    private static let userCancelled = -128

    static var target: URL { Bundle.main.bundleURL.appending(path: bundledCommand) }

    static var isInstalled: Bool { linkDestination() == target.path }

    /// A link with Aria's bundle layout: this copy, or one that moved.
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

    /// /usr/local/bin belongs to root, so macOS asks for an administrator's
    /// password. The script runs in this process, so the dialog names Aria
    /// rather than osascript.
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
