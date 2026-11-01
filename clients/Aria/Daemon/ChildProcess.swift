import Foundation
import Synchronization

/// A child process whose stdout and stderr drain as they arrive, so waiting
/// on it holds no thread and a full stderr pipe cannot stall its stdout.
final class ChildProcess: Sendable {
    struct Output: Sendable {
        var status: Int32 = 0
        var stdout = Data()
        var stderr = Data()
    }

    private struct State {
        var output = Output()
        var openPipes = 2
        var hasExited = false
        var waiters: [CheckedContinuation<Output, Never>] = []
    }

    private let process = Process()
    private let state = Mutex(State())

    init(_ executable: URL, _ arguments: [String]) {
        process.executableURL = executable
        process.arguments = arguments
    }

    /// Cancelling the task interrupts the child; what it printed until then
    /// is still returned.
    static func run(_ executable: URL, _ arguments: [String], input: Data? = nil) async throws -> Output {
        let child = ChildProcess(executable, arguments)
        try child.launch(input: input)
        return await child.exit()
    }

    /// With `onStdout`, stdout goes there in order and stays out of the output.
    func launch(input: Data? = nil, onStdout: (@Sendable (Data) -> Void)? = nil) throws {
        let stdin = input.map { _ in Pipe() }
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardInput = stdin ?? FileHandle.nullDevice
        process.standardOutput = stdout
        process.standardError = stderr
        process.terminationHandler = { [weak self] process in
            self?.update { state in
                state.output.status = process.terminationStatus
                state.hasExited = true
            }
        }
        try process.run()

        drain(stdout) { [weak self] chunk in
            if let onStdout {
                onStdout(chunk)
            } else {
                self?.update { $0.output.stdout.append(chunk) }
            }
        }
        drain(stderr) { [weak self] chunk in
            self?.update { $0.output.stderr.append(chunk) }
        }
        if let stdin, let input {
            do {
                try Self.write(input, to: stdin.fileHandleForWriting)
            } catch {
                interrupt()
                throw error
            }
        }
    }

    /// Cancellation interrupts the child, then waits for exit and both pipes
    /// to close so its final output is preserved.
    func exit() async -> Output {
        await withTaskCancellationHandler {
            await withCheckedContinuation { waiter in
                update { $0.waiters.append(waiter) }
            }
        } onCancel: {
            self.interrupt()
        }
    }

    func interrupt() {
        if process.isRunning { process.interrupt() }
    }

    private func update(_ change: (inout State) -> Void) {
        let finished: (waiters: [CheckedContinuation<Output, Never>], output: Output)? = state.withLock { state in
            change(&state)
            guard state.hasExited, state.openPipes == 0, !state.waiters.isEmpty else { return nil }
            defer { state.waiters = [] }
            return (state.waiters, state.output)
        }
        guard let finished else { return }
        for waiter in finished.waiters { waiter.resume(returning: finished.output) }
    }

    /// The pipe is read to its end even once nobody waits on the child, so
    /// the child never blocks on a full pipe.
    private func drain(_ pipe: Pipe, into sink: @escaping @Sendable (Data) -> Void) {
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty {
                handle.readabilityHandler = nil
                self?.update { $0.openPipes -= 1 }
            } else {
                sink(chunk)
            }
        }
    }

    private static func write(_ input: Data, to handle: FileHandle) throws {
        // A child that died before reading would otherwise take the app down
        // with SIGPIPE instead of failing the write.
        _ = fcntl(handle.fileDescriptor, F_SETNOSIGPIPE, 1)
        try handle.write(contentsOf: input)
        try handle.close()
    }
}
