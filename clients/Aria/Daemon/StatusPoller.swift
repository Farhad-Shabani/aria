import Foundation

/// Publishes daemon state for the menu bar. Polls every few seconds;
/// `working` bridges the gap while a start/stop command is in flight so
/// the UI can disable the toggle.
@MainActor
final class StatusPoller: ObservableObject {
    @Published private(set) var state: DaemonState = .stopped
    @Published private(set) var lastError: String?

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
        state = running ? .running : .stopped
    }

    /// The app open means the daemon runs: a stopped daemon is started once,
    /// as if the user had pressed the toggle.
    func ensureRunning() async {
        do {
            try await client.retireLegacyService()
        } catch {
            lastError = error.localizedDescription
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
        lastError = nil
        do {
            if shouldStart {
                try await client.start()
                state = await awaitRunning() ? .running : .stopped
            } else {
                try await client.stop()
                state = .stopped
            }
        } catch {
            lastError = error.localizedDescription
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
            case .stillRunning: return "Aria didn't stop in time."
            }
        }
    }

    /// Stops, waits for the process to exit, runs `whileStopped`, then
    /// starts. A daemon that won't exit is left running and `whileStopped`
    /// never runs. An error from `whileStopped` is rethrown after the start.
    func restart(whileStopped: () throws -> Void = {}) async throws {
        guard state != .working else { throw RestartFailure.busy }
        state = .working
        lastError = nil
        if await !client.hasExited() {
            do {
                try await client.stop()
            } catch {
                lastError = error.localizedDescription
            }
        }
        guard await awaitExit() else {
            state = await client.isRunning() ? .running : .stopped
            throw RestartFailure.stillRunning
        }
        let stopped = Result { try whileStopped() }
        do {
            try await client.start()
            lastError = nil
            state = await awaitRunning() ? .running : .stopped
        } catch {
            lastError = error.localizedDescription
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
        return false
    }
}
