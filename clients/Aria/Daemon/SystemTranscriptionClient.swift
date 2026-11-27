import Foundation
import GRPCCore
import GRPCProtobuf

struct SystemTranscriptionSnapshot: Sendable, Equatable {
    let isRunning: Bool
    /// The running session's file, or the last one's. `nil` before any session.
    let transcriptFile: URL?
}

enum SystemTranscriptionClient {
    static func start() async throws {
        try await DaemonTransport.call { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .start(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    /// Stop answers once the session's final decode has drained.
    static func stop() async throws {
        try await DaemonTransport.call(timeout: .seconds(30)) { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .stop(request: ClientRequest(message: .init()), options: options) { try $0.message }
        }
    }

    static func status() async throws -> SystemTranscriptionSnapshot {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .getStatus(request: ClientRequest(message: .init()), options: options) { response in
                    let message = try response.message
                    return SystemTranscriptionSnapshot(
                        isRunning: message.running, transcriptFile: transcriptFile(atPath: message.transcriptPath))
                }
        }
    }

    static func listUnsaved() async throws -> UnsavedTranscriptList {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .listUnsaved(request: ClientRequest(message: .init()), options: options) { response in
                    let message = try response.message
                    guard !message.unsavedDir.isEmpty else { throw DaemonFailure.invalidResponse }
                    let unsavedDir = URL(filePath: message.unsavedDir, directoryHint: .isDirectory)
                    return UnsavedTranscriptList(
                        transcripts: try message.transcripts.map { try UnsavedTranscript($0, in: unsavedDir) },
                        isHistoryFull: message.historyFull, unsavedDir: unsavedDir)
                }
        }
    }

    static func readUnsaved(_ transcript: UnsavedTranscript) async throws -> UnsavedTranscriptText {
        let sent = Aria_Daemon_V1_ReadUnsavedTranscriptRequest.with {
            $0.name = transcript.name
        }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .readUnsaved(request: ClientRequest(message: sent), options: options) { response in
                    let message = try response.message
                    switch message.read {
                    case .whole: return .whole(message.text)
                    case .truncated: return .truncated(message.text)
                    case .unspecified, .unavailable, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
                    }
                }
        }
    }

    static func discardUnsaved(_ transcript: UnsavedTranscript) async throws {
        let sent = Aria_Daemon_V1_DiscardUnsavedTranscriptRequest.with {
            $0.name = transcript.name
        }
        try await DaemonTransport.call { client, options in
            _ = try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .discardUnsaved(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }

    /// Returns whether the unsaved transcript stayed after its copy was saved.
    static func saveUnsaved(_ transcript: UnsavedTranscript, to destination: URL) async throws -> Bool {
        let sent = Aria_Daemon_V1_SaveUnsavedTranscriptRequest.with {
            $0.name = transcript.name
            $0.destination = destination.path(percentEncoded: false)
        }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_SystemTranscriptionService.Client(wrapping: client)
                .saveUnsaved(request: ClientRequest(message: sent), options: options) { try $0.message.kept }
        }
    }

    /// The daemon sends an empty path for no file.
    static func transcriptFile(atPath path: String) -> URL? {
        path.isEmpty ? nil : URL(filePath: path)
    }
}

extension UnsavedTranscript {
    /// A transcript the daemon listed in `folder`. Its name must be a plain file name there.
    init(_ wire: Aria_Daemon_V1_UnsavedTranscript, in folder: URL) throws {
        guard !wire.name.isEmpty, !wire.name.contains("/"), ![".", ".."].contains(wire.name), wire.hasStartedAt
        else { throw DaemonFailure.invalidResponse }
        self.init(
            name: wire.name, path: folder.appending(path: wire.name, directoryHint: .notDirectory),
            startedAt: wire.startedAt.date, duration: wire.duration.timeInterval)
    }
}
