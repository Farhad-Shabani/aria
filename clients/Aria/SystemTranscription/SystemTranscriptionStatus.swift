import Foundation

/// Whether a system transcription is running, whichever caller started it, and
/// the transcript followed so far.
@MainActor
final class SystemTranscriptionStatus: ObservableObject {
    static let stopSymbol = "stop.fill"
    static let savingNote = "Saving to transcript file"
    static let startFailure = "Aria couldn’t start transcribing your Mac’s audio. Try again."
    static let stopFailure = "Aria couldn’t stop transcribing. Try again."

    @Published private(set) var isRunning = false {
        didSet { if !isRunning { level.value = 0 } }
    }
    /// How loud the Mac's audio is. Silent whenever no session runs.
    let level = LiveLevel()
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
    /// A reply said no session runs before the stream did, so levels still
    /// queued belong to the session that ended. The next Started clears it.
    private var areLevelsStale = false
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
        events.onSystemAudioLevel { [weak self] level in self?.hear(level: level) }
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
            // A new stream replays no levels from before it.
            areLevelsStale = false
            return
        }
        transcript.apply(event, at: now)
        switch event.stage {
        case .started:
            didFailToStop = false
            areLevelsStale = false
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

    /// Only a running session sends levels, so one shows the session even
    /// before a stage or a status read does, as when Aria opens partway
    /// through. The daemon sends the last level before the session's end.
    func hear(level: Float) {
        guard !areLevelsStale else { return }
        // Levels come 20 times a second. Only the first may publish, or every
        // observer redraws with each one and every status read is dropped.
        if !isRunning {
            stagesSeen += 1
            isRunning = true
        }
        self.level.value = level
    }

    /// The stream doesn't replay a session that started before it connected.
    /// A read that fails leaves the state alone; the transport logs why.
    func catchUp() async {
        let stagesAtRead = stagesSeen
        guard let isRunning = try? await readStatus(), stagesSeen == stagesAtRead else { return }
        self.isRunning = isRunning
        areLevelsStale = !isRunning
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
            areLevelsStale = true
        } catch {
            await catchUp()
            didFailToStop = isRunning
        }
    }
}
