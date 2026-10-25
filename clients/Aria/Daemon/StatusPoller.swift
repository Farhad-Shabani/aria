import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "StatusPoller")

/// Why the last start or stop went wrong, in words a user can act on. The
/// underlying error goes to the log.
enum DaemonControlFailure: Equatable {
    case start
    case stop
    case restart
    case stillRunning
    case unresponsive
    case backgroundItemOff

    /// Aria turned off in Login Items is the one start failure the user can
    /// fix directly, so it gets its own message.
    static func starting(_ error: any Error, otherwise failure: Self) -> Self {
        if case DaemonClientError.agentDisabled = error { return .backgroundItemOff }
        return failure
    }

    var message: String {
        switch self {
        case .start: "Aria couldn't start. Run aria doctor in Terminal to see why."
        case .stop: "Aria couldn't stop. Run aria doctor in Terminal to see why."
        case .restart: "Aria couldn't restart. Run aria doctor in Terminal to see why."
        case .stillRunning: "Aria didn't stop in time."
        case .unresponsive: "Aria started but didn't respond."
        case .backgroundItemOff:
            "Aria is off in the background. Turn it on in System Settings → General → Login Items."
        }
    }
}

/// Publishes daemon state for the menu bar. Polls every few seconds;
/// `working` bridges the gap while a start/stop command is in flight so
/// the UI can disable the toggle.
@MainActor
final class StatusPoller: ObservableObject {
    @Published private(set) var state: DaemonState = .stopped
    @Published private(set) var failure: DaemonControlFailure?

    private let client: any DaemonControlling
    /// How often a start or stop is checked for, up to 20 times.
    private let probeInterval: Duration
    private var timer: Timer?

    init(client: any DaemonControlling, probeInterval: Duration = .milliseconds(500)) {
        self.client = client
        self.probeInterval = probeInterval
    }

    func startPolling(interval: TimeInterval = 3.0) {
        guard timer == nil else { return }
        Task { await ensureRunning() }
        let timer = Timer(timeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in await self?.pollOnce() }
        }
        // .common, not .default — menu tracking switches the run loop to
        // .eventTracking, which suspends default-mode timers while the
        // menu is held open.
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func pollOnce() async {
        guard state != .working else { return }
        let running = await client.isRunning()
        guard state != .working else { return }
        if running && state == .stopped {
            failure = nil
        }
        state = running ? .running : .stopped
    }

    /// The app open means the daemon runs: a stopped daemon is started once,
    /// as if the user had pressed the toggle.
    func ensureRunning() async {
        do {
            try await client.retireLegacyService()
        } catch {
            log.error("Retiring the legacy service failed: \(error.localizedDescription, privacy: .private)")
            failure = .start
            return
        }
        await pollOnce()
        if state == .stopped {
            await toggle()
        }
    }

    func toggle() async {
        guard state != .working else { return }
        let shouldStart = state != .running
        state = .working
        failure = nil
        do {
            if shouldStart {
                try await client.start()
                state = await awaitRunning() ? .running : .stopped
            } else {
                try await client.stop()
                state = .stopped
            }
        } catch {
            log.error(
                "Daemon \(shouldStart ? "start" : "stop", privacy: .public) failed: \(error.localizedDescription, privacy: .private)"
            )
            failure = shouldStart ? .starting(error, otherwise: .start) : .stop
            let running = await client.isRunning()
            state = running ? .running : .stopped
        }
    }

    enum RestartFailure: LocalizedError {
        case busy
        case stillRunning

        var errorDescription: String? {
            switch self {
            case .busy: return "Aria is busy starting or stopping."
            case .stillRunning: return DaemonControlFailure.stillRunning.message
            }
        }
    }

    /// Stops, waits for the process to exit, runs `whileStopped`, then
    /// starts. A daemon that won't exit is left running and `whileStopped`
    /// never runs. An error from `whileStopped` is rethrown after the start.
    func restart(whileStopped: () throws -> Void = {}) async throws {
        guard state != .working else { throw RestartFailure.busy }
        state = .working
        failure = nil
        if await !client.hasExited() {
            do {
                try await client.stop()
            } catch {
                log.error("Daemon stop before restart failed: \(error.localizedDescription, privacy: .private)")
                failure = .restart
            }
        }
        guard await awaitExit() else {
            failure = .stillRunning
            state = await client.isRunning() ? .running : .stopped
            throw RestartFailure.stillRunning
        }
        let stopped = Result { try whileStopped() }
        do {
            try await client.start()
            failure = nil
            state = await awaitRunning() ? .running : .stopped
        } catch {
            log.error("Daemon start after restart failed: \(error.localizedDescription, privacy: .private)")
            failure = .starting(error, otherwise: .restart)
            state = await client.isRunning() ? .running : .stopped
        }
        try stopped.get()
    }

    private func awaitExit() async -> Bool {
        for _ in 0..<20 {
            if await client.hasExited() { return true }
            try? await Task.sleep(for: probeInterval)
        }
        return false
    }

    /// A launch returns before the daemon listens. `working` holds until
    /// the socket accepts, so `running` means preferences can be read.
    private func awaitRunning() async -> Bool {
        for _ in 0..<20 {
            if await client.isRunning() { return true }
            try? await Task.sleep(for: probeInterval)
        }
        log.error("Daemon launched but its socket never accepted")
        failure = .unresponsive
        return false
    }
}
