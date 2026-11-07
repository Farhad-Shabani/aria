import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "TranscriptionEngineChange")

/// Sets dictation's speech-to-text engine. The daemon swaps the engine once
/// it sees the settings write, so the current one keeps working until then.
@MainActor
final class TranscriptionEngineChange: ObservableObject {
    struct Effects {
        var run:
            (
                Aria_Setup_V1_SetTranscriptionEngineRequest,
                @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void
            ) async throws -> Aria_Setup_V1_SetTranscriptionEngineSummary
    }

    enum State: Equatable {
        case idle
        case downloading(ModelTransfer?)
        /// Setup checks the models or writes the settings, which takes a moment.
        case finishing
        case failed(String)
    }

    static let failure = "Aria couldn’t change the speech model. Try again."

    private struct Run {
        let engine: String
        var state: State
    }

    /// Kept after a failure, so the picker row can say which change failed.
    @Published private var run: Run?

    var engine: String? { run?.engine }
    var state: State { run?.state ?? .idle }

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

    /// `ended` runs however the run ends, even cancelled, since an interrupt
    /// can land after the settings were written.
    func start(_ engine: String, then ended: @escaping @MainActor () async -> Void) {
        guard task == nil else { return }
        run = Run(engine: engine, state: .finishing)
        task = Task {
            defer { task = nil }
            var request = Aria_Setup_V1_SetTranscriptionEngineRequest()
            request.engineID = engine
            do {
                _ = try await runner.run {
                    try await effects.run(request) { event in
                        Task { @MainActor in self.absorb(event) }
                    }
                }
            } catch  where Task.isCancelled {
            } catch SetupClientError.cancelled {
            } catch {
                log.error("Speech engine change failed: \(String(describing: error), privacy: .private)")
                run?.state = .failed(SetupClientError.message(for: error, otherwise: Self.failure))
                await ended()
                return
            }
            await ended()
            run = nil
        }
    }

    func cancel() {
        task?.cancel()
    }

    private func absorb(_ event: Aria_Setup_V1_SetupEvent) {
        guard isRunning else { return }
        switch event.event {
        case .started(.models): run?.state = .downloading(nil)
        case .download(let progress):
            if let transfer = ModelTransfer(progress) { run?.state = .downloading(transfer) }
        case .started(.settings): run?.state = .finishing
        case .started, nil: break
        }
    }
}

extension TranscriptionEngineChange.Effects {
    static var live: Self {
        .init(run: { request, onEvent in try await SetupClient.setTranscriptionEngine(request, onEvent: onEvent) })
    }
}
