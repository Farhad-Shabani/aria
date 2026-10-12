import Foundation

enum DaemonState: Equatable {
    case running
    case stopped
    case working
}

protocol DaemonControlling: Sendable {
    func isRunning() async -> Bool
    func start() async throws
    func stop() async throws
}

enum DaemonClientError: LocalizedError {
    case ariaBinaryMissing(String)
    case commandFailed(String)

    var errorDescription: String? {
        switch self {
        case .ariaBinaryMissing(let path): return "aria binary not found at \(path)"
        case .commandFailed(let message): return message
        }
    }
}

/// How `start()` brings the daemon up.
enum StartMethod: Equatable {
    /// `launchctl kickstart` of the login service, which then owns the process.
    case kickstart
    /// A child `aria daemon` at this binary, owned by nobody but the lock.
    case spawn(URL)
}

/// Starts the registered launchd service, or spawns the CLI when no service exists.
/// Probes the Unix socket for liveness and stops the PID recorded in the lock file.
/// Runtime paths and service identity mirror the Rust daemon.
struct DaemonClient: DaemonControlling {
    static let serviceLabel = "com.aria.daemon"

    static var servicePlistURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/\(serviceLabel).plist")
    }

    /// The service wins whenever it is registered, so one supervisor owns the
    /// process and a crash restarts it. Without it, the first executable
    /// candidate is spawned; none means the build is missing.
    static func startMethod(serviceInstalled: Bool, executableCandidates: [URL]) -> StartMethod? {
        if serviceInstalled { return .kickstart }
        return executableCandidates.first.map(StartMethod.spawn)
    }

    /// Parses the lock-file PID; missing or invalid contents return nil.
    static func lockedPid(_ contents: String?) -> pid_t? {
        guard let contents else { return nil }
        return pid_t(contents.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// Where `aria daemon` lives. The `ariaDaemonBinary` default overrides
    /// everything and is trusted verbatim, so a wrong override fails loudly
    /// with that exact path instead of silently falling back. Otherwise the
    /// app runs the `aria` it carries in `Contents/Helpers`.
    static func daemonBinaryCandidates(
        bundledExecutable: URL? = Bundle.main.executableURL,
        defaults: UserDefaults = .standard
    ) -> [URL] {
        if let stored = defaults.string(forKey: "ariaDaemonBinary") {
            return [URL(fileURLWithPath: (stored as NSString).expandingTildeInPath)]
        }
        // The CLI ships in Contents/Helpers, one directory over from the
        // app binary, so both can be named for Aria on one disk.
        let bundled = bundledExecutable.map {
            $0.deletingLastPathComponent()
                .deletingLastPathComponent()
                .appendingPathComponent("Helpers/aria")
        }
        return [bundled].compactMap { $0 }
    }

    func isRunning() async -> Bool {
        let path = RuntimeFiles.daemonSocketURL.path
        return await Task.detached(priority: .utility) {
            socketAccepts(path: path)
        }.value
    }

    func start() async throws {
        let candidates = Self.daemonBinaryCandidates()
        let executables = candidates.filter {
            FileManager.default.isExecutableFile(atPath: $0.path)
        }
        let installed = FileManager.default.fileExists(atPath: Self.servicePlistURL.path)
        switch Self.startMethod(serviceInstalled: installed, executableCandidates: executables) {
        case .kickstart:
            try Self.kickstartService()
        case .spawn(let bin):
            try Self.spawnDaemon(at: bin)
        case nil:
            throw DaemonClientError.ariaBinaryMissing(
                candidates.map(\.path).joined(separator: ", ")
            )
        }
    }

    /// A no-op for a job that already has its process, so a launch race with
    /// the service costs nothing.
    private static func kickstartService() throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        process.arguments = ["kickstart", "gui/\(getuid())/\(serviceLabel)"]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        let stderr = Pipe()
        process.standardError = stderr
        do {
            try process.run()
        } catch {
            throw DaemonClientError.commandFailed(
                "launchctl failed to launch: \(error.localizedDescription)"
            )
        }
        let output = stderr.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            let message = String(decoding: output, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw DaemonClientError.commandFailed(
                "launchctl kickstart exited with \(process.terminationStatus): \(message)"
            )
        }
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
            throw DaemonClientError.commandFailed(
                "aria daemon failed to launch: \(error.localizedDescription)"
            )
        }
    }

    /// Checks that the lock is held and the recorded PID names an `aria` executable.
    /// These checks reduce stale-PID risk but do not make signaling atomic.
    func stop() async throws {
        try Self.signalStop()
    }

    /// Synchronous so the app can call it while terminating, when no task
    /// gets to resume.
    static func signalStop() throws {
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
        let fd = open(lock.path, O_RDONLY)
        guard fd >= 0 else {
            throw DaemonClientError.commandFailed(
                "daemon is not running: no lock file at \(lock.path)"
            )
        }
        defer { close(fd) }
        if flock(fd, LOCK_EX | LOCK_NB) == 0 {
            flock(fd, LOCK_UN)
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
