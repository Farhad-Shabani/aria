import Foundation

enum RuntimeFiles {
    /// A file in the Rust daemon's runtime directory:
    /// `$ARIA_RUNTIME_DIR`, else `~/.aria/daemon`. Mirrors
    /// `RuntimePaths::for_current_user` in crates/aria/src/runtime.rs.
    ///
    /// A launchd app inherits no shell environment, so the override only
    /// applies when the app itself was launched with it set.
    static func daemonRuntimeURL(_ name: String) -> URL {
        let directory =
            ProcessInfo.processInfo.environment["ARIA_RUNTIME_DIR"]
            ?? NSHomeDirectory() + "/.aria/daemon"
        return URL(fileURLWithPath: directory).appendingPathComponent(name)
    }

    // The names are the daemon's contract, pinned by a test in
    // crates/aria/src/runtime.rs. Every reference goes through these two.
    static var daemonSocketURL: URL { daemonRuntimeURL("daemon.sock") }
    static var daemonLockURL: URL { daemonRuntimeURL("daemon.lock") }
}
