import Foundation
import ServiceManagement
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonClient")

enum DaemonState: Equatable {
    case running
    case stopped
    case working
}

protocol DaemonControlling: Sendable {
    func retireLegacyService() async throws
    func isRunning() async -> Bool
    /// The daemon process is gone: nothing holds its lock.
    func hasExited() async -> Bool
    func start() async throws
    func stop() async throws
}

enum DaemonClientError: LocalizedError {
    case ariaBinaryMissing(String)
    case agentDisabled
    case commandFailed(String)

    var errorDescription: String? {
        switch self {
        case .ariaBinaryMissing(let path): return "aria binary not found at \(path)"
        case .agentDisabled: return "the Login Items background item is off"
        case .commandFailed(let message): return message
        }
    }
}

enum StartMethod: Equatable {
    /// `launchctl kickstart` of the app's login agent, which then owns the process.
    case kickstart
    /// Registers the agent first, so launchd loads its job, then kickstarts it.
    case register
    /// A child `aria daemon` at this binary, owned by nobody but the lock.
    case spawn(URL)
}

/// Starts the daemon through the app's login agent, or spawns an overridden
/// binary during development. Probes the Unix socket for liveness and stops
/// the PID recorded in the lock file. Runtime paths and the agent's label
/// mirror the Rust daemon.
struct DaemonClient: DaemonControlling {
    private static let serviceLabel = "io.github.farhad-shabani.aria.daemon"
    private static let legacyRetiredKey = "legacyServiceRetired"
    private static let agentLoadProbes = 20
    private static let agentLoadPoll = Duration.milliseconds(100)

    /// Bundled in `Contents/Library/LaunchAgents`. Without RunAtLoad it
    /// starts only when the app asks, so quitting Aria keeps it stopped.
    private static var agent: SMAppService { .agent(plistName: "\(serviceLabel).plist") }

    private static var agentTarget: String { "gui/\(getuid())/\(serviceLabel)" }
    private static var launchctl: URL { URL(fileURLWithPath: "/bin/launchctl") }

    /// What `aria service install` writes. It claims the agent's label.
    private static var legacyServicePlistURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/\(serviceLabel).plist")
    }

    /// An overridden binary always spawns, so a development build runs
    /// without registering anything. Otherwise the agent runs the daemon,
    /// and one the user turned off stays off. An enabled agent can still
    /// lose its job, to `launchctl bootout` for one; registering reloads it.
    static func startMethod(agentStatus: SMAppService.Status, jobLoaded: Bool, override: URL?) throws -> StartMethod {
        if let override { return .spawn(override) }
        switch agentStatus {
        case .enabled: return jobLoaded ? .kickstart : .register
        case .notRegistered, .notFound: return .register
        case .requiresApproval: throw DaemonClientError.agentDisabled
        @unknown default: throw DaemonClientError.agentDisabled
        }
    }

    static func lockedPid(_ contents: String?) -> pid_t? {
        guard let contents else { return nil }
        return pid_t(contents.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private static func daemonBinaryOverride(defaults: UserDefaults = .standard) -> URL? {
        defaults.string(forKey: "ariaDaemonBinary").map {
            URL(fileURLWithPath: ($0 as NSString).expandingTildeInPath)
        }
    }

    /// Where `aria daemon` lives. The `ariaDaemonBinary` default overrides
    /// everything and is trusted verbatim, so a wrong override fails loudly
    /// with that exact path instead of silently falling back. Otherwise the
    /// app runs the `aria` inside its nested `Aria.app` helper.
    static func daemonBinaryCandidates(
        bundledExecutable: URL? = Bundle.main.executableURL,
        defaults: UserDefaults = .standard
    ) -> [URL] {
        if let override = daemonBinaryOverride(defaults: defaults) {
            return [override]
        }
        let bundled = bundledExecutable.map {
            $0.deletingLastPathComponent()
                .deletingLastPathComponent()
                .appendingPathComponent("Helpers/Aria.app/Contents/MacOS/aria")
        }
        return [bundled].compactMap { $0 }
    }

    static func ariaBinary() throws -> URL {
        let candidates = daemonBinaryCandidates()
        guard let bin = candidates.first(where: { FileManager.default.isExecutableFile(atPath: $0.path) }) else {
            throw DaemonClientError.ariaBinaryMissing(candidates.map(\.path).joined(separator: ", "))
        }
        return bin
    }

    static func setPhonePairing(_ enabled: Bool) async throws {
        try await run(try ariaBinary(), ["phone", enabled ? "enable" : "disable"])
    }

    /// The invitation is a secret, so it goes over stdin, never the arguments.
    static func pairServer(invitation: String) async throws {
        let binary = try ariaBinary()
        let output: ChildProcess.Output
        do {
            output = try await ChildProcess.run(binary, ["remote", "pair"], input: Data(invitation.utf8))
        } catch {
            throw launchFailed("aria remote", error)
        }
        guard output.status == 0 else {
            throw DaemonClientError.commandFailed(pairFailure(stderr: output.stderr))
        }
    }

    private static let cliErrorPrefix = "Error: "

    static func pairFailure(stderr: Data) -> String {
        let lines = String(decoding: stderr, as: UTF8.self).split(whereSeparator: \.isNewline)
        var message = lines.first.map(String.init) ?? ""
        if message.hasPrefix(cliErrorPrefix) {
            message.removeFirst(cliErrorPrefix.count)
        }
        guard let first = message.first else { return "Pairing failed." }
        let sentence = first.uppercased() + message.dropFirst()
        return sentence.hasSuffix(".") ? sentence : sentence + "."
    }

    /// Removes the service older setups installed, once, since it shares the
    /// agent's label. Call it before reading the agent's status. A service
    /// the user installs later stays theirs.
    func retireLegacyService() async throws {
        let defaults = UserDefaults.standard
        guard Self.daemonBinaryOverride() == nil, !defaults.bool(forKey: Self.legacyRetiredKey) else { return }
        if FileManager.default.fileExists(atPath: Self.legacyServicePlistURL.path) {
            // The helper's `service uninstall` boots the job out and deletes its plist.
            try await Self.run(try Self.ariaBinary(), ["service", "uninstall"])
        }
        defaults.set(true, forKey: Self.legacyRetiredKey)
    }

    func isRunning() async -> Bool {
        let path = RuntimeFiles.daemonSocketURL.path
        return await Task.detached(priority: .utility) {
            socketAccepts(path: path)
        }.value
    }

    func hasExited() async -> Bool {
        let lock = RuntimeFiles.daemonLockURL
        return await Task.detached(priority: .utility) {
            !Self.lockIsHeld(lock)
        }.value
    }

    func start() async throws {
        let method = try Self.startMethod(
            agentStatus: Self.agent.status,
            jobLoaded: await Self.agentLoaded(),
            override: Self.daemonBinaryOverride()
        )
        switch method {
        case .kickstart:
            log.notice("Starting the daemon: kickstart")
            try await Self.kickstartAgent()
        case .register:
            log.notice("Starting the daemon: register")
            do {
                try Self.agent.register()
            } catch {
                throw DaemonClientError.commandFailed(
                    "could not register Aria's background item: \(error.localizedDescription)"
                )
            }
            await Self.awaitAgentLoaded()
            try await Self.kickstartAgent()
        case .spawn(let bin):
            log.notice("Starting the daemon: spawn")
            guard FileManager.default.isExecutableFile(atPath: bin.path) else {
                throw DaemonClientError.ariaBinaryMissing(bin.path)
            }
            try Self.spawnDaemon(at: bin)
        }
    }

    /// A no-op for a job that already has its process, so a launch race with
    /// the agent costs nothing.
    private static func kickstartAgent() async throws {
        try await run(launchctl, ["kickstart", agentTarget])
    }

    /// `register()` returns before launchd loads the job, and a kickstart
    /// that comes too soon finds no job. One that never loads is left for
    /// the kickstart to report.
    private static func awaitAgentLoaded() async {
        for _ in 0..<agentLoadProbes {
            if await agentLoaded() { return }
            try? await Task.sleep(for: agentLoadPoll)
        }
    }

    private static func agentLoaded() async -> Bool {
        (try? await run(launchctl, ["print", agentTarget])) != nil
    }

    private static func run(_ executable: URL, _ arguments: [String]) async throws {
        let name = ([executable.lastPathComponent] + arguments.prefix(1)).joined(separator: " ")
        let output: ChildProcess.Output
        do {
            output = try await ChildProcess.run(executable, arguments)
        } catch {
            throw launchFailed(name, error)
        }
        guard output.status == 0 else {
            throw exitFailed(name, status: output.status, stderr: output.stderr)
        }
    }

    private static func launchFailed(_ name: String, _ error: any Error) -> DaemonClientError {
        .commandFailed("\(name) failed to launch: \(error.localizedDescription)")
    }

    private static func exitFailed(_ name: String, status: Int32, stderr: Data) -> DaemonClientError {
        let message = String(decoding: stderr, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        return .commandFailed("\(name) exited with \(status): \(message)")
    }

    private static func spawnDaemon(at bin: URL) throws {
        let process = Process()
        process.executableURL = bin
        process.arguments = ["daemon"]
        // Detached: the daemon logs to its own file and outlives this app.
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            throw launchFailed("aria daemon", error)
        }
    }

    func stop() async throws {
        if Self.stopsThroughAgent {
            do {
                try await Self.run(Self.launchctl, Self.killAgentArguments)
                return
            } catch {
                Self.logKillFallback(error)
            }
        }
        try Self.signalLockHolder()
    }

    /// `stop()` that blocks the calling thread, for the one caller that
    /// cannot await: `applicationWillTerminate`, where no task gets to resume.
    static func signalStop() throws {
        if stopsThroughAgent {
            do {
                try killAgentBlocking()
                return
            } catch {
                logKillFallback(error)
            }
        }
        try signalLockHolder()
    }

    /// The agent's daemon reads the login shell's PATH before it takes its
    /// lock; launchd reaches it then too. A job with no process fails the
    /// kill and falls to the lock, which reports why.
    private static var stopsThroughAgent: Bool { daemonBinaryOverride() == nil }

    private static func logKillFallback(_ error: any Error) {
        log.notice("launchctl kill failed, trying the lock: \(error.localizedDescription, privacy: .private)")
    }

    private static var killAgentArguments: [String] { ["kill", "SIGTERM", agentTarget] }

    private static func killAgentBlocking() throws {
        let process = Process()
        process.executableURL = launchctl
        process.arguments = killAgentArguments
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        let stderr = Pipe()
        process.standardError = stderr
        do {
            try process.run()
        } catch {
            throw launchFailed("launchctl kill", error)
        }
        let errors = stderr.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw exitFailed("launchctl kill", status: process.terminationStatus, stderr: errors)
        }
    }

    /// Checks the lock and executable before signaling. These checks reduce
    /// stale-PID risk but do not make signaling atomic.
    private static func signalLockHolder() throws {
        let lock = RuntimeFiles.daemonLockURL
        let pid = try Self.heldLockPid(lock)
        guard Self.isAriaProcess(pid) else {
            throw DaemonClientError.commandFailed(
                "PID \(pid) from \(lock.path) is not an aria process"
            )
        }
        guard kill(pid, SIGTERM) == 0 else {
            throw DaemonClientError.commandFailed(
                "failed to signal daemon PID \(pid): \(String(cString: strerror(errno)))"
            )
        }
    }

    /// The PID recorded in a lock file some process still holds. A free
    /// lock means no owner — its contents are a leftover, not a PID.
    private static func heldLockPid(_ lock: URL) throws -> pid_t {
        guard FileManager.default.fileExists(atPath: lock.path) else {
            throw DaemonClientError.commandFailed(
                "daemon is not running: no lock file at \(lock.path)"
            )
        }
        guard lockIsHeld(lock) else {
            throw DaemonClientError.commandFailed(
                "daemon is not running: nothing holds \(lock.path)"
            )
        }
        let contents = try? String(contentsOf: lock, encoding: .utf8)
        guard let pid = lockedPid(contents) else {
            throw DaemonClientError.commandFailed(
                "no daemon PID recorded at \(lock.path)"
            )
        }
        return pid
    }

    private static func lockIsHeld(_ lock: URL) -> Bool {
        let fd = open(lock.path, O_RDONLY)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else { return true }
        flock(fd, LOCK_UN)
        return false
    }

    private static func isAriaProcess(_ pid: pid_t) -> Bool {
        var buffer = [UInt8](repeating: 0, count: Int(MAXPATHLEN))
        guard proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0 else { return false }
        let path = String(decoding: buffer.prefix { $0 != 0 }, as: UTF8.self)
        return URL(fileURLWithPath: path).lastPathComponent == "aria"
    }
}

/// Blocking connect probe — a listener that accepts is a live daemon.
/// Call off the main thread.
private func socketAccepts(path: String) -> Bool {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { return false }
    defer { close(fd) }

    var timeout = timeval(tv_sec: 2, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let fits = withUnsafeMutableBytes(of: &addr.sun_path) { raw -> Bool in
        let bytes = Array(path.utf8)
        guard bytes.count < raw.count else { return false }
        raw.copyBytes(from: bytes)
        return true
    }
    guard fits else { return false }

    let result = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    return result == 0
}
