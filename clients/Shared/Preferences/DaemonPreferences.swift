import Foundation
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonPreferences")

/// Synchronizes daemon preferences through one revision and pending patch.
/// The transport decides which daemon and over what; this class does not.
@MainActor
final class DaemonPreferences: ObservableObject {
    enum UpdateError: LocalizedError {
        case conflict
        case invalidResponse

        var errorDescription: String? {
            switch self {
            case .conflict: "Settings kept changing. Aria will save your changes on the next sync."
            case .invalidResponse: "Aria didn’t confirm your changes. Try again."
            }
        }
    }

    /// Nil until the daemon answers.
    @Published private(set) var snapshot: Aria_Daemon_V1_UserPreferences?
    /// The last failed sync or push; cleared by the next one that succeeds.
    @Published private(set) var lastError: (any Error)?

    private let transport: any PreferencesTransport
    private var revision: UInt64?
    /// Changes the daemon has not accepted yet; the next sync pushes them.
    private var pending = Aria_Daemon_V1_PreferencePatch()
    private var isPushing = false

    init(transport: any PreferencesTransport) {
        self.transport = transport
    }

    var isHistoryEnabled: Bool {
        snapshot?.dictationHistory ?? true
    }

    func sync() async {
        guard !isPushing else { return }
        do {
            let snapshot = try await transport.snapshot()
            guard !isPushing else { return }
            // A snapshot fetched before a push landed is older than what
            // we already know; adopting it would revert the push.
            guard snapshot.revision >= revision ?? 0 else { return }
            revision = snapshot.revision
            lastError = nil
            if pending == Aria_Daemon_V1_PreferencePatch() {
                adopt(snapshot.preferences)
            } else {
                await push()
            }
        } catch {
            // Closing the window cancels its sync; the next one reports the daemon.
            guard !Task.isCancelled else { return }
            log.error("Sync failed: \(String(describing: error), privacy: .private)")
            lastError = error
        }
    }

    /// Shows the edit at once and pushes it; the daemon's answer replaces it.
    func set(_ edit: (inout Aria_Daemon_V1_PreferencePatch) -> Void) async {
        edit(&pending)
        if var shown = snapshot {
            pending.apply(to: &shown)
            snapshot = shown
        }
        await push()
    }

    /// Lays unaccepted edits over the daemon's snapshot, so a reply to an
    /// older push never flips a control the user has since moved.
    private func adopt(_ accepted: Aria_Daemon_V1_UserPreferences) {
        var shown = accepted
        pending.apply(to: &shown)
        snapshot = shown
    }

    /// A status that says the daemon will never take this push as sent, as
    /// opposed to the daemon being away.
    private static func refusesPatch(_ error: any Error) -> Bool {
        guard let status = error as? RPCError else { return false }
        switch status.code {
        case .invalidArgument, .permissionDenied, .unauthenticated: return true
        default: return false
        }
    }

    private func restore(_ sent: Aria_Daemon_V1_PreferencePatch) {
        var combined = sent
        pending.apply(to: &combined)
        pending = combined
    }

    private func push() async {
        guard !isPushing else { return }
        isPushing = true
        defer { isPushing = false }
        var conflicts = 0
        while pending != Aria_Daemon_V1_PreferencePatch() {
            let sent = pending
            pending = Aria_Daemon_V1_PreferencePatch()
            do {
                try Task.checkCancellation()
                if revision == nil {
                    revision = try await transport.snapshot().revision
                }
                try Task.checkCancellation()
                var request = Aria_Daemon_V1_UpdatePreferencesRequest()
                request.expectedRevision = revision ?? 0
                request.patch = sent
                switch try await transport.update(request).result {
                case .snapshot(let accepted):
                    revision = accepted.revision
                    lastError = nil
                    adopt(accepted.preferences)
                case .conflict(let conflict):
                    revision = conflict.current.revision
                    restore(sent)
                    adopt(conflict.current.preferences)
                    conflicts += 1
                    if conflicts == 2 {
                        log.error("Preference update stopped after repeated revision conflicts")
                        lastError = UpdateError.conflict
                        return
                    }
                case nil:
                    throw UpdateError.invalidResponse
                }
            } catch {
                log.error("Update failed: \(String(describing: error), privacy: .private)")
                lastError = error
                if Self.refusesPatch(error) {
                    // Drop only the refused request; edits made during it still need a push.
                    do {
                        let fresh = try await transport.snapshot()
                        revision = fresh.revision
                        adopt(fresh.preferences)
                    } catch {
                        log.error("Preference recovery failed: \(String(describing: error), privacy: .private)")
                    }
                } else {
                    restore(sent)
                }
                return
            }
        }
    }
}
