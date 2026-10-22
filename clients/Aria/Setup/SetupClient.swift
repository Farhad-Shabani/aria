import AppKit
import Foundation
import ServiceManagement
import Synchronization
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupClient")

enum SetupClientError: LocalizedError, Equatable {
    case cancelled
    case failed(String)
    case invalidOutput(SetupFrames.FrameError)

    var errorDescription: String? {
        switch self {
        case .cancelled: return "Setup was cancelled."
        case .failed(let message): return message
        case .invalidOutput(let error): return error.localizedDescription
        }
    }
}

/// Aria.app's side of `aria setup`: one child per call, `aria.setup.v1`
/// frames on its stdio. The same binary the daemon runs from, so the
/// service it installs and the Accessibility grant name one executable.
enum SetupClient {
    static func inspect() async throws -> Aria_Setup_V1_Inspection {
        let bin = try DaemonClient.ariaBinary()
        return try await Task.detached(priority: .userInitiated) {
            let process = Process()
            process.executableURL = bin
            process.arguments = ["setup", "inspect"]
            let stdout = Pipe()
            let stderr = Pipe()
            process.standardInput = FileHandle.nullDevice
            process.standardOutput = stdout
            process.standardError = stderr
            try process.run()
            let output = stdout.fileHandleForReading.readDataToEndOfFile()
            let errors = stderr.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                throw SetupClientError.failed(failure(status: process.terminationStatus, stderr: errors))
            }
            var reader = SetupFrames.Reader()
            let frames = try reader.push(output)
            try reader.finish()
            guard let frame = frames.first else {
                throw SetupClientError.failed(failure(status: process.terminationStatus, stderr: errors))
            }
            return try Aria_Setup_V1_Inspection(serializedBytes: frame)
        }.value
    }

    /// Runs `aria setup apply`, handing each event over as it arrives.
    /// Cancelling the task interrupts the child, which keeps a partial
    /// download for the next run.
    static func apply(
        _ request: Aria_Setup_V1_ApplyRequest,
        onEvent: @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void
    ) async throws -> Aria_Setup_V1_Summary {
        let bin = try DaemonClient.ariaBinary()
        let frame = try SetupFrames.encode(request)
        let child = Mutex(Child())
        let run = Task.detached(priority: .userInitiated) { () throws -> Run in
            let process = Process()
            process.executableURL = bin
            process.arguments = ["setup", "apply"]
            let stdin = Pipe()
            let stdout = Pipe()
            let stderr = Pipe()
            process.standardInput = stdin
            process.standardOutput = stdout
            process.standardError = stderr
            try process.run()
            defer {
                // A frame that cannot be read leaves nothing to drive the
                // child; it must not finish setup on its own.
                if process.isRunning {
                    process.interrupt()
                    process.waitUntilExit()
                }
                child.withLock { state in state.pid = 0 }
            }
            let interruptNow = child.withLock { state in
                state.pid = process.processIdentifier
                return state.interrupted
            }
            if interruptNow { process.interrupt() }
            // A child that died before reading would otherwise take the
            // app down with SIGPIPE instead of failing the write.
            let request = stdin.fileHandleForWriting
            _ = fcntl(request.fileDescriptor, F_SETNOSIGPIPE, 1)
            try request.write(contentsOf: frame)
            try request.close()

            var reader = SetupFrames.Reader()
            var summary: Aria_Setup_V1_Summary?
            var frameError: SetupFrames.FrameError?
            let output = stdout.fileHandleForReading
            while summary == nil {
                let chunk = output.availableData
                if chunk.isEmpty {
                    do throws(SetupFrames.FrameError) {
                        try reader.finish()
                    } catch {
                        frameError = error
                    }
                    break
                }
                for bytes in try reader.push(chunk) {
                    switch try Aria_Setup_V1_ApplyResponse(serializedBytes: bytes).body {
                    case .event(let event): onEvent(event)
                    case .summary(let done): summary = done
                    case nil: break
                    }
                }
            }
            let errors = stderr.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            return Run(summary: summary, status: process.terminationStatus, stderr: errors, frameError: frameError)
        }
        let finished = try await withTaskCancellationHandler {
            try await run.value
        } onCancel: {
            child.withLock { state in
                state.interrupted = true
                if state.pid != 0 { kill(state.pid, SIGINT) }
            }
        }
        return try outcome(finished, cancelled: Task.isCancelled).get()
    }

    struct Run: Sendable {
        var summary: Aria_Setup_V1_Summary?
        var status: Int32
        var stderr: Data
        var frameError: SetupFrames.FrameError? = nil
    }

    /// A summary means the run finished, whatever the exit; short of one, an
    /// interrupt the app asked for is a cancellation, even with partial output.
    static func outcome(_ run: Run, cancelled: Bool) -> Result<Aria_Setup_V1_Summary, SetupClientError> {
        if let summary = run.summary { return .success(summary) }
        if cancelled { return .failure(.cancelled) }
        if run.status == 0, let error = run.frameError { return .failure(.invalidOutput(error)) }
        return .failure(.failed(failure(status: run.status, stderr: run.stderr)))
    }

    private struct Child {
        var pid: pid_t = 0
        var interrupted = false
    }

    /// Waits for a signalled daemon to let go of the socket. One that will
    /// not is left for the service install to refuse loudly.
    static func awaitDaemonSocketClosed(probes: Int = 40, every: Duration = .milliseconds(250)) async {
        let client = DaemonClient()
        for _ in 0..<probes {
            if await !client.isRunning() { return }
            try? await Task.sleep(for: every)
        }
    }

    private static func failure(status: Int32, stderr: Data) -> String {
        let message = String(decoding: stderr, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        return message.isEmpty ? "aria setup exited with \(status)" : message
    }
}

extension SetupState.Effects {
    @MainActor static var live: Self {
        .init(
            inspect: { try await SetupClient.inspect() },
            apply: { request, onEvent in try await SetupClient.apply(request, onEvent: onEvent) },
            stopDaemon: {
                do {
                    try DaemonClient.signalStop()
                } catch {
                    log.error("Daemon not stopped before apply: \(String(describing: error), privacy: .private)")
                    return
                }
                await SetupClient.awaitDaemonSocketClosed()
            },
            startDaemon: {
                do {
                    // A new Task, so a cancelled apply cannot cut short the
                    // agent-load wait inside `start`.
                    try await Task { try await DaemonClient().start() }.value
                } catch {
                    log.error("Daemon not restarted after apply: \(String(describing: error), privacy: .private)")
                }
            },
            hotkeyArmed: { await HealthClient.serving(HealthClient.hotkeyService) },
            openAccessibility: {
                let pane = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
            },
            speakers: { try await SpeakerClient.list() },
            enroll: { name in try await SpeakerClient.enroll(name: name) },
            transcribe: { try await TryOutClient.transcribe() },
            speak: { text in try await TryOutClient.speak(text) },
            launchAtLogin: {
                do {
                    try SMAppService.mainApp.register()
                    return nil
                } catch {
                    return "Launch at login was not enabled: \(error.localizedDescription)"
                }
            },
            installCommandLineTool: {
                do {
                    if try CommandLineTool.install() { return nil }
                    return "The aria command was not installed; run Set Up Aria… again to add it."
                } catch {
                    return "The aria command was not installed: \(error.localizedDescription)"
                }
            }
        )
    }
}
