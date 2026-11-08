import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Palette")

/// The prompt the palette runs on a history entry. One at a time: starting
/// another cancels the one before.
@MainActor
final class PaletteRun: ObservableObject {
    struct Effects {
        /// Runs a prompt on a history entry, typing nothing, and returns its
        /// ticket. The daemon reads the entry in full and saves the reply.
        var run: (_ entry: UInt64, _ source: PromptSource) async throws -> UInt64
        var cancel: (UInt64) async throws -> Void
    }

    private static let keptTickets = 16

    /// The run the result page shows. `nil` once it is canceled or the palette closes.
    @Published private(set) var result: PaletteResult?
    private var task: Task<Void, Never>?
    private var generation = 0
    /// Tickets of runs started here, kept a while after so their late events
    /// stay out of the pill too.
    private var tickets: [UInt64] = []
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    func start(_ source: PromptSource, named name: String, on input: HistoryItem) {
        cancel()
        generation += 1
        let generation = generation
        result = PaletteResult(promptName: name, source: source, input: input)
        task = Task {
            do {
                let ticket = try await effects.run(input.id, source)
                remember(ticket)
                guard !Task.isCancelled else {
                    cancel(ticket: ticket)
                    return
                }
                guard generation == self.generation else { return }
                result?.started(ticket: ticket)
            } catch {
                guard generation == self.generation, !Task.isCancelled else { return }
                result?.fail(Self.failure(for: error))
            }
        }
    }

    /// Runs the same prompt on the same entry again, once the last one ended.
    func retry() {
        guard let result, !result.isRunning, let source = result.source else { return }
        start(source, named: result.promptName, on: result.input)
    }

    func show(saved result: PromptResult, on input: HistoryItem) {
        cancel()
        self.result = PaletteResult(saved: result, on: input)
    }

    /// Stops the prompt, which then saves nothing.
    func cancel() {
        task?.cancel()
        task = nil
        if let result, result.isRunning, let ticket = result.ticket { cancel(ticket: ticket) }
        result = nil
    }

    /// The palette closed: the run goes on unseen, and the daemon saves its
    /// reply in the entry's thread.
    func release() {
        generation += 1
        task = nil
        result = nil
    }

    func apply(_ event: Aria_Daemon_V1_ChatEvent?) {
        result?.apply(event)
    }

    /// Whether the pill should leave this ticket's events to the palette.
    func shows(ticket: UInt64) -> Bool {
        tickets.contains(ticket)
    }

    private func remember(_ ticket: UInt64) {
        tickets.append(ticket)
        if tickets.count > Self.keptTickets { tickets.removeFirst() }
    }

    private func cancel(ticket: UInt64) {
        Task {
            do {
                try await effects.cancel(ticket)
            } catch {
                log.error("Canceling a prompt failed: \(String(describing: error), privacy: .private)")
            }
        }
    }

    private static func failure(for error: Error) -> PaletteResult.Failure {
        if let failure = error as? HistoryFailure { return .init(failure.message) }
        let failure = error as? DaemonFailure
        if failure?.isOffline == true { return .init(DaemonFailure.offlineMessage) }
        if case .refused(let code)? = failure { return .init(code: code) }
        return .init(PaletteResult.failed)
    }
}
