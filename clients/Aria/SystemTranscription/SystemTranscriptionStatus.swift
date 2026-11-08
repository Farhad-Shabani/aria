import Foundation

/// Whether a system transcription is running, whichever caller started it, and
/// the transcript followed so far.
@MainActor
final class SystemTranscriptionStatus: ObservableObject {
    static let stopSymbol = "stop.fill"
    static let savingNote = "Saving to transcript file"
    static let startFailure = "Aria couldn’t start transcribing your Mac’s audio. Try again."
    static let stopFailure = "Aria couldn’t stop transcribing. Try again."

    @Published private(set) var isRunning = false
    @Published private(set) var isStarting = false
    @Published private(set) var didFailToStart = false
    @Published private(set) var isStopping = false
    @Published private(set) var didFailToStop = false
    @Published private(set) var transcript = SystemTranscript()
    /// When the session began, worked out from the first stage seen. `nil`
    /// until a stage arrives, as for a session that began before Aria opened.
    @Published private(set) var startedAt: Date?

    typealias ReadStatus = @Sendable () async throws -> Bool
    typealias StartSession = @Sendable () async throws -> Void
    typealias StopSession = @Sendable () async throws -> Void

    private let readStatus: ReadStatus
    private let startSession: StartSession
    private let stopSession: StopSession
    /// Goes up with every stream update, so a status read that one overtook is dropped.
    private var stagesSeen = 0
    private let reconcileInterval: Duration
    private var reconciling: Task<Void, Never>?

    init(
        readStatus: @escaping ReadStatus = SystemTranscriptionClient.isRunning,
        startSession: @escaping StartSession = SystemTranscriptionClient.start,
        stopSession: @escaping StopSession = SystemTranscriptionClient.stop,
        reconcileInterval: Duration = .seconds(15)
    ) {
        self.readStatus = readStatus
        self.startSession = startSession
        self.stopSession = stopSession
        self.reconcileInterval = reconcileInterval
    }

    convenience init(events: DaemonEvents) {
        self.init()
        events.onSystemTranscription { [weak self] event in self?.handle(event) }
        events.onStreamOpened { [weak self] in self?.startReconciling() }
    }

    deinit {
        reconciling?.cancel()
    }

    /// Stages can be lost, and a session that panics emits no ending, so the
    /// daemon's status keeps correcting the state while the stream is up.
    func startReconciling() {
        reconciling?.cancel()
        reconciling = Task { [weak self, reconcileInterval] in
            while !Task.isCancelled {
                await self?.catchUp()
                try? await Task.sleep(for: reconcileInterval)
            }
        }
    }

    /// `nil` means the stream is down. The daemon may be gone, so the state
    /// says no session until a later read or stage reports one.
    func handle(_ event: Aria_Daemon_V1_SystemTranscriptionEvent?, now: Date = Date()) {
        stagesSeen += 1
        guard let event else {
            reconciling?.cancel()
            isRunning = false
            return
        }
        transcript.apply(event, at: now)
        switch event.stage {
        case .started:
            didFailToStop = false
            isRunning = true
            startedAt = now
        case .segmentTranscribed, .speakerMerged:
            isRunning = true
            if startedAt == nil {
                startedAt = now.addingTimeInterval(-event.offset.timeInterval)
            }
        case .stopped, .failed:
            isRunning = false
        case .unspecified, .UNRECOGNIZED:
            break
        }
    }

    /// The stream doesn't replay a session that started before it connected.
    /// A read that fails leaves the state alone; the transport logs why.
    func catchUp() async {
        let stagesAtRead = stagesSeen
        guard let isRunning = try? await readStatus(), stagesSeen == stagesAtRead else { return }
        self.isRunning = isRunning
    }

    /// Answers once the daemon accepted the session. Capture can still fail
    /// after that, which the FAILED stage reports.
    func start() async {
        guard !isRunning, !isStarting else { return }
        isStarting = true
        defer { isStarting = false }
        didFailToStart = false
        do {
            try await startSession()
            isRunning = true
        } catch {
            didFailToStart = true
        }
    }

    /// A session that failed while stopping reports that failure from Stop, yet
    /// has ended. Only a session that outlives the call counts as a failed stop.
    func stop() async {
        guard !isStopping else { return }
        isStopping = true
        defer { isStopping = false }
        didFailToStop = false
        do {
            try await stopSession()
            isRunning = false
        } catch {
            await catchUp()
            didFailToStop = isRunning
        }
    }
}
