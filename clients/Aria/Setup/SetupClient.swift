import AppKit
import Foundation
import ServiceManagement
import SwiftProtobuf
import Synchronization
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupClient")

/// Detail for the log. Callers write their own text for the user.
enum SetupClientError: Error, Equatable {
    case cancelled
    /// Another setup run holds the lock, here or in Terminal.
    case busy
    case failed(String)
    case invalidOutput(SetupFrames.FrameError)

    static let busyMessage = "Aria is finishing another change. Try again when it’s done."

    /// The code `aria` exits with while another setup run holds the lock.
    static let busyStatus: Int32 = 75

    /// What a failed run tells the person: why it's busy, else `fallback`.
    static func message(for error: any Error, otherwise fallback: String) -> String {
        error as? SetupClientError == .busy ? busyMessage : fallback
    }
}

/// Aria.app's side of `aria setup`: one child per call, `aria.setup.v1`
/// frames on its stdio. The same binary the daemon runs from, so the
/// service it installs and the Accessibility grant name one executable.
enum SetupClient {
    static func removeModel(_ id: String) async throws {
        let output = try await ChildProcess.run(try DaemonProcess.ariaBinary(), ["models", "remove", id])
        guard output.status == 0 else {
            throw failure(status: output.status, stderr: output.stderr)
        }
    }

    static func inspect() async throws -> Aria_Setup_V1_Inspection {
        let output = try await ChildProcess.run(try DaemonProcess.ariaBinary(), ["setup", "inspect"])
        guard output.status == 0 else {
            throw failure(status: output.status, stderr: output.stderr)
        }
        var reader = SetupFrames.Reader()
        let frames = try reader.push(output.stdout)
        try reader.finish()
        guard let frame = frames.first else {
            throw failure(status: output.status, stderr: output.stderr)
        }
        return try Aria_Setup_V1_Inspection(serializedBytes: frame)
    }

    /// Runs `aria setup apply`, handing each event over as it arrives.
    /// Cancelling the task interrupts the child, which keeps a partial
    /// download for the next run.
    static func apply(
        _ request: Aria_Setup_V1_ApplyRequest,
        onEvent: @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void
    ) async throws -> Aria_Setup_V1_Summary {
        try await run("apply", request, onEvent: onEvent) { bytes in
            switch try Aria_Setup_V1_ApplyResponse(serializedBytes: bytes).body {
            case .event(let event): .event(event)
            case .summary(let summary): .summary(summary)
            case nil: nil
            }
        }
    }

    /// Fetches the engine's missing models, then writes the settings that name
    /// it. Cancelling interrupts the run as it interrupts `apply`.
    static func setTranscriptionEngine(
        _ request: Aria_Setup_V1_SetTranscriptionEngineRequest,
        onEvent: @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void
    ) async throws -> Aria_Setup_V1_SetTranscriptionEngineSummary {
        try await run("set-transcription-engine", request, onEvent: onEvent) { bytes in
            switch try Aria_Setup_V1_SetTranscriptionEngineResponse(serializedBytes: bytes).body {
            case .event(let event): .event(event)
            case .summary(let summary): .summary(summary)
            case nil: nil
            }
        }
    }

    enum Response<Summary> {
        case event(Aria_Setup_V1_SetupEvent)
        case summary(Summary)
    }

    private static func run<Summary: Sendable>(
        _ command: String,
        _ request: some SwiftProtobuf.Message,
        onEvent: @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void,
        decode: @escaping @Sendable (Data) throws -> Response<Summary>?
    ) async throws -> Summary {
        let frame = try SetupFrames.encode(request)
        let child = ChildProcess(try DaemonProcess.ariaBinary(), ["setup", command])
        let frames = Mutex(ResponseFrames<Summary>(decode: decode))
        try child.launch(input: frame) { chunk in
            let readable = frames.withLock { $0.push(chunk, onEvent: onEvent) }
            // A frame that cannot be read leaves nothing to drive the
            // child; it must not finish on its own.
            if !readable { child.interrupt() }
        }
        let output = await child.exit()
        let finished = try frames.withLock { try $0.run(output) }
        return try outcome(finished, cancelled: Task.isCancelled).get()
    }

    private struct ResponseFrames<Summary: Sendable>: Sendable {
        let decode: @Sendable (Data) throws -> Response<Summary>?
        var reader = SetupFrames.Reader()
        var summary: Summary?
        var unreadable: (any Error)?

        /// False on the push that finds an unreadable frame.
        mutating func push(_ chunk: Data, onEvent: (Aria_Setup_V1_SetupEvent) -> Void) -> Bool {
            guard summary == nil, unreadable == nil else { return true }
            do {
                for bytes in try reader.push(chunk) {
                    switch try decode(bytes) {
                    case .event(let event): onEvent(event)
                    case .summary(let done): summary = done
                    case nil: break
                    }
                }
            } catch {
                unreadable = error
            }
            return unreadable == nil
        }

        mutating func run(_ output: ChildProcess.Output) throws -> Run<Summary> {
            if let unreadable { throw unreadable }
            var frameError: SetupFrames.FrameError?
            if summary == nil {
                do throws(SetupFrames.FrameError) {
                    try reader.finish()
                } catch {
                    frameError = error
                }
            }
            return Run(summary: summary, status: output.status, stderr: output.stderr, frameError: frameError)
        }
    }

    struct Run<Summary: Sendable>: Sendable {
        var summary: Summary?
        var status: Int32
        var stderr: Data
        var frameError: SetupFrames.FrameError? = nil
    }

    /// A summary means the run finished, whatever the exit; short of one, an
    /// interrupt the app asked for is a cancellation, even with partial output.
    static func outcome<Summary>(_ run: Run<Summary>, cancelled: Bool) -> Result<Summary, SetupClientError> {
        if let summary = run.summary { return .success(summary) }
        if cancelled { return .failure(.cancelled) }
        if run.status == 0, let error = run.frameError { return .failure(.invalidOutput(error)) }
        return .failure(failure(status: run.status, stderr: run.stderr))
    }

    static func openAccessibility() {
        let pane = "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility"
        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
    }

    static func openKeyboard() {
        let pane = "x-apple.systempreferences:com.apple.Keyboard-Settings.extension"
        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
    }

    /// Waits for a signalled daemon to let go of the socket. One that will
    /// not is left for the service install to refuse loudly.
    static func awaitDaemonSocketClosed(probes: Int = 40, every: Duration = .milliseconds(250)) async {
        let client = DaemonProcess()
        for _ in 0..<probes {
            if await !client.isRunning() { return }
            try? await Task.sleep(for: every)
        }
    }

    /// Without a service, the app runs its own daemon, which holds the socket
    /// the service needs. It stops first and returns if apply fails.
    @MainActor static func handingOffDaemon<Result>(
        _ inspection: Aria_Setup_V1_Inspection,
        stop: () async -> Void,
        start: () async -> Void,
        apply: () async throws -> Result
    ) async throws -> Result {
        let stopsDaemon = inspection.service.job == .absent
        if stopsDaemon { await stop() }
        do {
            return try await apply()
        } catch {
            if stopsDaemon { await start() }
            throw error
        }
    }

    static func stopDaemonBeforeApply() async {
        do {
            try await DaemonProcess().stop()
        } catch {
            log.error("Daemon not stopped before apply: \(String(describing: error), privacy: .private)")
            return
        }
        await awaitDaemonSocketClosed()
    }

    static func startDaemonAfterFailedApply() async {
        do {
            // A new Task, so a cancelled apply cannot cut short the
            // agent-load wait inside `start`.
            try await Task { try await DaemonProcess().start() }.value
        } catch {
            log.error("Daemon not restarted after apply: \(String(describing: error), privacy: .private)")
        }
    }

    private static func failure(status: Int32, stderr: Data) -> SetupClientError {
        if status == SetupClientError.busyStatus { return .busy }
        let message = String(decoding: stderr, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        return .failed(message.isEmpty ? "aria setup exited with \(status)" : message)
    }
}

extension SetupState.Effects {
    @MainActor static var live: Self {
        .init(
            inspect: { try await SetupClient.inspect() },
            apply: { request, onEvent in try await SetupClient.apply(request, onEvent: onEvent) },
            stopDaemon: { await SetupClient.stopDaemonBeforeApply() },
            startDaemon: { await SetupClient.startDaemonAfterFailedApply() },
            hotkeyArmed: { await HealthClient.isServing(HealthClient.hotkeyService) },
            openAccessibility: { SetupClient.openAccessibility() },
            speakers: { try await SpeakerClient.list() },
            enroll: { name in try await SpeakerClient.enroll(name: name) },
            transcribe: { try await TryOutClient.transcribe() },
            speak: { text in try await SpeechClient.speak(text) },
            launchAtLogin: {
                do {
                    try SMAppService.mainApp.register()
                    return nil
                } catch {
                    log.error("Open at login not turned on: \(String(describing: error), privacy: .private)")
                    return "Aria couldn’t set itself to open at login. Turn on Open at login in Settings > General."
                }
            },
            installCommandLineTool: {
                do {
                    if try CommandLineTool.install() { return nil }
                    return CommandLineTool.notInstalled
                } catch {
                    log.error("aria command not installed: \(String(describing: error), privacy: .private)")
                    return (error as? CommandLineToolError)?.localizedDescription ?? CommandLineTool.notInstalled
                }
            }
        )
    }
}
