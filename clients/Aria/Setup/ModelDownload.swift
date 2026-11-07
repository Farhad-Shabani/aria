import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "ModelDownload")

/// Fetches one model where Finish Setting Up Aria lists it. It runs the same
/// `aria setup apply` as Aria Setup, which also writes the current choices
/// back and installs the background service.
@MainActor
final class ModelDownload: ObservableObject {
    struct Effects {
        var inspect: () async throws -> Aria_Setup_V1_Inspection
        var apply:
            (
                Aria_Setup_V1_ApplyRequest,
                @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void
            ) async throws -> Aria_Setup_V1_Summary
        var stopDaemon: () async -> Void
        var startDaemon: () async -> Void
    }

    enum State: Equatable {
        case idle
        case downloading(ModelTransfer?)
        /// The model is in place. Setup installs the service and checks the Aria key.
        case finishing
        case failed(String)
    }

    static let failure = "Aria couldn’t download the speech model. Try again."

    private struct Run {
        let model: String
        var state: State
    }

    /// The model the run fetches, so only its own row shows the run.
    @Published private var run: Run?

    var model: String? { run?.model }
    var state: State { run?.state ?? .idle }

    func state(of model: String) -> State {
        run?.model == model ? state : .idle
    }

    var isRunning: Bool {
        switch state {
        case .downloading, .finishing: true
        case .idle, .failed: false
        }
    }

    private let effects: Effects
    private let runner: SetupRunner
    private var task: Task<Void, Never>?

    init(effects: Effects, runner: SetupRunner) {
        self.effects = effects
        self.runner = runner
    }

    /// `downloaded` runs only once setup finishes, never after a failure.
    func start(_ modelID: String, then downloaded: @escaping @MainActor () async -> Void) {
        guard task == nil else { return }
        run = Run(model: modelID, state: .downloading(nil))
        task = Task {
            defer { task = nil }
            do {
                try await runner.run { try await fetch(modelID) }
                await downloaded()
                reset()
            } catch  where Task.isCancelled {
                reset()
            } catch SetupClientError.cancelled {
                reset()
            } catch {
                log.error("Model download failed: \(String(describing: error), privacy: .private)")
                run?.state = .failed(SetupClientError.message(for: error, otherwise: Self.failure))
            }
        }
    }

    /// Reads the choices now: apply writes them back, and an older inspection
    /// would undo a change made since.
    private func fetch(_ modelID: String) async throws {
        let inspection = try await effects.inspect()
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = inspection.defaults
        request.downloads = [modelID]
        _ = try await SetupClient.handingOffDaemon(
            inspection, stop: effects.stopDaemon, start: effects.startDaemon
        ) {
            try await effects.apply(request) { event in
                Task { @MainActor in self.absorb(event) }
            }
        }
    }

    func cancel() {
        task?.cancel()
    }

    private func reset() {
        run = nil
    }

    private func absorb(_ event: Aria_Setup_V1_SetupEvent) {
        guard case .downloading = state else { return }
        switch event.event {
        case .download(let progress):
            if let transfer = ModelTransfer(progress) { run?.state = .downloading(transfer) }
        case .started(.service): run?.state = .finishing
        case .started, nil: break
        }
    }
}

extension ModelDownload.Effects {
    static var live: Self {
        .init(
            inspect: { try await SetupClient.inspect() },
            apply: { request, onEvent in try await SetupClient.apply(request, onEvent: onEvent) },
            stopDaemon: { await SetupClient.stopDaemonBeforeApply() },
            startDaemon: { await SetupClient.startDaemonAfterFailedApply() }
        )
    }
}
