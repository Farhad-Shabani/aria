import Foundation

/// Publishes daemon state for the menu bar. Polls every few seconds;
/// `working` bridges the gap while a start/stop command is in flight so
/// the UI can disable the toggle.
@MainActor
final class StatusPoller: ObservableObject {
    @Published private(set) var state: DaemonState = .stopped
    @Published private(set) var lastError: String?

    private let client: any DaemonControlling
    private var timer: Timer?

    init(client: any DaemonControlling) {
        self.client = client
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

    /// Stops, waits for the socket to close, and starts again. Under launchd
    /// the service may be back before `start`, which is then a no-op.
    func restart() async {
        guard state != .working else { return }
        let wasRunning = state == .running
        state = .working
        lastError = nil
        do {
            if wasRunning {
                try await client.stop()
                _ = await awaitSocket(accepting: false)
            }
            try await client.start()
            state = await awaitRunning() ? .running : .stopped
        } catch {
            lastError = error.localizedDescription
            let running = await client.isRunning()
            state = running ? .running : .stopped
        }
    }

    /// A launch returns before the daemon listens. `working` holds until
    /// the socket accepts, so `running` means preferences can be read.
    private func awaitRunning() async -> Bool {
        await awaitSocket(accepting: true)
    }

    private func awaitSocket(accepting: Bool, probes: Int = 20, every: Duration = .milliseconds(500)) async -> Bool {
        for _ in 0..<probes {
            if await client.isRunning() == accepting { return true }
            try? await Task.sleep(for: every)
        }
        return false
    }
}
