import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "UnsavedTranscripts")

/// A Mac Audio transcript no history entry names, waiting for the person to save or discard it.
struct UnsavedTranscript: Equatable, Sendable {
    /// The file's name, which the daemon's Read and Discard take.
    let name: String
    let path: URL
    let startedAt: Date
    /// How long the session wrote to it.
    let duration: TimeInterval
}

struct UnsavedTranscriptList: Sendable {
    var transcripts: [UnsavedTranscript]
    /// Whether Mac Audio history is full, so new sessions run unsaved.
    var isHistoryFull: Bool
    var transcriptsDir: URL
    var unsavedDir: URL
}

/// An unsaved transcript's text, as the daemon reads it.
enum UnsavedTranscriptText: Equatable, Sendable {
    case whole(String)
    /// The first MiB, ending on a whole character.
    case truncated(String)
    /// Gone, or not readable.
    case unreadable
    /// Aria isn’t running, so nothing read it.
    case unavailable
}

/// The unsaved transcripts the daemon lists, and saving or discarding one.
@MainActor
final class UnsavedTranscripts: ObservableObject {
    nonisolated static let title = "Unsaved Transcript"
    nonisolated static let historyFullNotice = "History is full. Delete transcripts so new ones go to history."
    nonisolated static let goneNotice = "This transcript is no longer there. Aria updated the list."

    struct Effects {
        var list: () async throws -> UnsavedTranscriptList
        var discard: (UnsavedTranscript) async throws -> Void
        var read: (UnsavedTranscript) async throws -> UnsavedTranscriptText
        /// Copies the transcript’s file over whatever is at the destination.
        var copy: (_ transcript: UnsavedTranscript, _ destination: URL) async throws -> Void
    }

    /// Oldest first, as the daemon lists them.
    @Published private(set) var transcripts: [UnsavedTranscript] = []
    @Published private(set) var isHistoryFull = false
    /// Aria’s folders, as the last list named them.
    private var ariaDirs: [URL] = []
    /// Why the last save, discard, or read failed.
    @Published private(set) var error: String?
    /// Why the last list failed, so the person knows a transcript may be missing from it.
    @Published private(set) var listFailure: String?
    /// One change at a time.
    @Published private(set) var isChanging = false
    /// Goes up with every change to the list, so a list read that finishes after a newer change is dropped.
    private var listChanges = 0
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    convenience init(events: DaemonEvents) {
        self.init(effects: .live)
        events.onStreamOpened { [weak self] in self?.spawnRefresh() }
        events.onSystemTranscription { [weak self] event in self?.handle(event) }
    }

    /// A session that ends may leave a transcript unsaved.
    func handle(_ event: Aria_Daemon_V1_SystemTranscriptionEvent?) {
        guard let event, [.stopped, .failed].contains(event.stage) else { return }
        spawnRefresh()
    }

    func spawnRefresh() {
        Task { await refresh() }
    }

    /// A read that fails leaves the list as it was, and `listFailure` says so.
    func refresh() async {
        listChanges += 1
        let change = listChanges
        do {
            let list = try await effects.list()
            guard change == listChanges else { return }
            transcripts = list.transcripts
            isHistoryFull = list.isHistoryFull
            ariaDirs = [list.transcriptsDir, list.unsavedDir]
            listFailure = nil
        } catch {
            // A page that left mid-list cancelled it, which is no failure.
            guard change == listChanges, !Task.isCancelled else { return }
            log.error("Listing unsaved transcripts failed: \(String(describing: error), privacy: .private)")
            listFailure = Self.message(for: error, otherwise: "Aria couldn’t list your unsaved transcripts. Try again.")
        }
    }

    /// Copies the transcript to `destination`, then discards it. Returns whether it was discarded.
    @discardableResult
    func save(_ transcript: UnsavedTranscript, to destination: URL) async -> Bool {
        guard begin() else { return false }
        defer { isChanging = false }
        // Saving it beside itself would put the copy where Discard then removes it. The check
        // resolves links on disk, which may be a slow volume, so it runs off the main actor.
        let (folder, aria) = (destination.deletingLastPathComponent(), ariaDirs)
        let isInAria = await Task.detached { Self.isInAriaFolders(folder, aria: aria) }.value
        guard !isInAria else {
            error = "Choose a place outside Aria’s transcript folders."
            return false
        }
        do {
            try await effects.copy(transcript, destination)
        } catch {
            log.error("Copying an unsaved transcript failed: \(String(describing: error), privacy: .private)")
            if Self.isGone(error) {
                showGone()
            } else {
                self.error = "Aria couldn’t save the transcript there. Choose another place."
            }
            return false
        }
        do {
            try await effects.discard(transcript)
        } catch {
            log.error("Discarding a copied transcript failed: \(String(describing: error), privacy: .private)")
            if Self.isGone(error) {
                self.error = "Aria saved a copy. This transcript changed since, so Aria left it."
                spawnRefresh()
                return false
            }
            if case .refused(.historyUnavailable) = error as? DaemonFailure {
                self.error = "Aria saved a copy. Click **Discard** to remove this one once history opens."
                return false
            }
            // The copy exists whatever the discard failure, so the message says that first.
            let offline = (error as? DaemonFailure)?.isOffline == true ? " \(DaemonFailure.offlineMessage)" : ""
            self.error = "Aria saved a copy but couldn’t remove this one. Click **Discard** to remove it.\(offline)"
            return false
        }
        remove(transcript)
        return true
    }

    func text(of transcript: UnsavedTranscript) async -> UnsavedTranscriptText {
        do {
            return try await effects.read(transcript)
        } catch {
            log.error("Reading an unsaved transcript failed: \(String(describing: error), privacy: .private)")
            if (error as? DaemonFailure)?.isOffline == true { return .unavailable }
            if Self.isGone(error) { showGone() }
            return .unreadable
        }
    }

    /// Returns whether it was discarded. `error` says why not.
    @discardableResult
    func discard(_ transcript: UnsavedTranscript) async -> Bool {
        guard begin() else {
            error = "Aria is still saving this transcript. Try again when it’s done."
            return false
        }
        defer { isChanging = false }
        do {
            try await effects.discard(transcript)
        } catch {
            log.error("Discarding an unsaved transcript failed: \(String(describing: error), privacy: .private)")
            if case .refused(.historyUnavailable) = error as? DaemonFailure {
                self.error = "Aria can’t discard this transcript until history opens. Try again later."
            } else if Self.isGone(error) {
                showGone()
            } else {
                self.error = Self.message(for: error, otherwise: "Aria couldn’t discard the transcript. Try again.")
            }
            return false
        }
        remove(transcript)
        return true
    }

    /// An error belongs to the page it was shown on.
    func clearError() {
        error = nil
    }

    /// Whether `folder` is one of Aria’s transcript folders, `aria`, or inside one, however a link
    /// spells it. Aria removes files there, so a copy saved there isn’t the person’s own.
    nonisolated static func isInAriaFolders(_ folder: URL, aria: [URL]) -> Bool {
        [folder.standardizedFileURL, folder.resolvingSymlinksInPath()].contains { start in
            var current = start.standardizedFileURL
            while current.pathComponents.count > 1 {
                if aria.contains(where: { isSameFolder(current, $0) }) { return true }
                current = current.deletingLastPathComponent().standardizedFileURL
            }
            return false
        }
    }

    /// Whether `a` and `b` are one folder however each is spelled, by file identity when both
    /// exist. Each resolves its links first, since a link’s own resource values describe the
    /// link, not its folder.
    nonisolated static func isSameFolder(_ a: URL, _ b: URL) -> Bool {
        let (a, b) = (a.resolvingSymlinksInPath().standardizedFileURL, b.resolvingSymlinksInPath().standardizedFileURL)
        let identity = { (url: URL) in
            try? url.resourceValues(forKeys: [.fileResourceIdentifierKey]).fileResourceIdentifier
        }
        if let left = identity(a), let right = identity(b) { return left.isEqual(right) }
        return a == b
    }

    private func begin() -> Bool {
        guard !isChanging else { return false }
        isChanging = true
        error = nil
        return true
    }

    private func remove(_ transcript: UnsavedTranscript) {
        listChanges += 1
        transcripts.removeAll { $0.name == transcript.name }
    }

    /// The transcript is gone since the daemon listed it, so the list is out of date.
    private static func isGone(_ error: Error) -> Bool {
        if case .refused(.unsavedTranscriptMissing) = error as? DaemonFailure { return true }
        return false
    }

    private func showGone() {
        error = Self.goneNotice
        spawnRefresh()
    }

    private static func message(for error: Error, otherwise message: String) -> String {
        guard let failure = error as? DaemonFailure, failure.isOffline else { return message }
        return DaemonFailure.offlineMessage
    }
}

extension UnsavedTranscripts.Effects {
    static var live: Self {
        .init(
            list: { try await SystemTranscriptionClient.listUnsaved() },
            discard: { try await SystemTranscriptionClient.discardUnsaved($0) },
            read: { try await SystemTranscriptionClient.readUnsaved($0) },
            copy: { transcript, destination in try await copy(transcript, to: destination) })
    }

    /// The save panel already asked before replacing a file, so one at `destination` is replaced.
    /// The copy is made on its volume first, so a copy that fails partway leaves nothing there.
    static func copy(_ transcript: UnsavedTranscript, to destination: URL) async throws {
        try await Task.detached {
            let source = try openSource(transcript.path)
            defer { try? source.close() }
            let files = FileManager.default
            let scratch = try files.url(
                for: .itemReplacementDirectory, in: .userDomainMask, appropriateFor: destination, create: true)
            defer { try? files.removeItem(at: scratch) }
            let copy = scratch.appending(path: destination.lastPathComponent)
            guard
                files.createFile(
                    atPath: copy.path(percentEncoded: false), contents: nil, attributes: [.posixPermissions: 0o600])
            else { throw CocoaError(.fileWriteUnknown) }
            let written = try FileHandle(forWritingTo: copy)
            defer { try? written.close() }
            // A chunk at a time, each freed before the next, so a long transcript never sits in
            // memory whole.
            while try autoreleasepool(invoking: {
                guard let chunk = try source.read(upToCount: 1 << 20), !chunk.isEmpty else { return false }
                try written.write(contentsOf: chunk)
                return true
            }) {}
            try written.synchronize()
            if files.fileExists(atPath: destination.path(percentEncoded: false)) {
                _ = try files.replaceItemAt(destination, withItemAt: copy)
            } else {
                try files.moveItem(at: copy, to: destination)
            }
        }.value
    }

    /// Opens `path` to read as the daemon does: never through a link, and only a regular file of
    /// the person’s. The copy reads the same handle it checked.
    private static func openSource(_ path: URL) throws -> FileHandle {
        let descriptor = open(path.path(percentEncoded: false), O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        var status = stat()
        guard fstat(descriptor, &status) == 0, status.st_mode & S_IFMT == S_IFREG, status.st_uid == getuid() else {
            close(descriptor)
            throw CocoaError(.fileReadNoPermission)
        }
        return FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
    }
}
