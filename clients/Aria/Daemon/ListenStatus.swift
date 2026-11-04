import Foundation

/// Whether a listen session is running, whichever caller started it.
@MainActor
final class ListenStatus: ObservableObject {
    @Published private(set) var isRunning = false
    @Published private(set) var isStopping = false
    @Published private(set) var didFailToStop = false

    typealias ReadStatus = @Sendable () async throws -> Bool
    typealias StopSession = @Sendable () async throws -> Void

    private let readStatus: ReadStatus
    private let stopSession: StopSession
    /// Goes up with every stream update, so a status read that one overtook is dropped.
    private var stagesSeen = 0
    private let reconcileInterval: Duration
    private var reconciling: Task<Void, Never>?

    init(
        readStatus: @escaping ReadStatus = ListenClient.isListening,
        stopSession: @escaping StopSession = ListenClient.stop,
        reconcileInterval: Duration = .seconds(15)
    ) {
        self.readStatus = readStatus
        self.stopSession = stopSession
        self.reconcileInterval = reconcileInterval
    }

    convenience init(events: DaemonEvents) {
        self.init()
        events.onListen { [weak self] event in self?.handle(event) }
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
    func handle(_ event: Aria_Daemon_V1_ListenEvent?) {
        stagesSeen += 1
        guard let event else {
            reconciling?.cancel()
            isRunning = false
            return
        }
        switch event.stage {
        case .started:
            didFailToStop = false
            isRunning = true
        case .segmentTranscribed, .speakerMerged:
            isRunning = true
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
