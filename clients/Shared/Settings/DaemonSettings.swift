import Foundation
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonSettings")

/// Synchronizes daemon settings through one revision and pending patch.
/// The transport decides which daemon and over what; this class does not.
@MainActor
final class DaemonSettings: ObservableObject {
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
    @Published private(set) var snapshot: Aria_Daemon_V1_Settings?
    /// The last failed sync or push; cleared by the next one that succeeds.
    @Published private(set) var lastError: (any Error)?

    private let transport: any SettingsTransport
    private var revision: UInt64?
    /// Changes the daemon has not accepted yet; the next sync pushes them.
    private var pending = Aria_Daemon_V1_SettingsPatch()
    private var isPushing = false
    /// A fetch that overlaps a push may predate it. The revision can't tell:
    /// it's a hash of the file's values, so it has no order.
    private var pushCount = 0
    private var fetchCount = 0
    private var newestFetchSeen = 0

    init(transport: any SettingsTransport) {
        self.transport = transport
    }

    /// The last push failed, and the change waits for the next sync.
    var hasUnsavedChange: Bool {
        lastError != nil && pending != Aria_Daemon_V1_SettingsPatch()
    }

    var isHistoryEnabled: Bool {
        snapshot?.historyEnabled ?? true
    }

    func sync() async {
        guard !isPushing else { return }
        let pushesBefore = pushCount
        fetchCount += 1
        let fetch = fetchCount
        do {
            let snapshot = try await transport.snapshot()
            // A snapshot fetched before a push landed is older than what
            // we already know; adopting it would revert the push.
            guard !isPushing, pushCount == pushesBefore, fetch > newestFetchSeen else { return }
            newestFetchSeen = fetch
            revision = snapshot.revision
            lastError = nil
            if pending == Aria_Daemon_V1_SettingsPatch() {
                adopt(snapshot.settings)
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
    func set(_ edit: (inout Aria_Daemon_V1_SettingsPatch) -> Void) async {
        edit(&pending)
        if var shown = snapshot {
            pending.apply(to: &shown)
            snapshot = shown
        }
        await push()
    }

    /// Lays unaccepted edits over the daemon's snapshot, so a reply to an
    /// older push never flips a control the user has since moved.
    private func adopt(_ accepted: Aria_Daemon_V1_Settings) {
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

    private func restore(_ sent: Aria_Daemon_V1_SettingsPatch) {
        var combined = sent
        pending.apply(to: &combined)
        pending = combined
    }

    private func push() async {
        guard !isPushing else { return }
        isPushing = true
        pushCount += 1
        defer { isPushing = false }
        var conflicts = 0
        while pending != Aria_Daemon_V1_SettingsPatch() {
            let sent = pending
            pending = Aria_Daemon_V1_SettingsPatch()
            do {
                try Task.checkCancellation()
                if revision == nil {
                    revision = try await transport.snapshot().revision
                }
                try Task.checkCancellation()
                var request = Aria_Daemon_V1_UpdateSettingsRequest()
                request.expectedRevision = revision ?? 0
                request.patch = sent
                switch try await transport.update(request).result {
                case .snapshot(let accepted):
                    revision = accepted.revision
                    lastError = nil
                    adopt(accepted.settings)
                case .conflict(let conflict):
                    revision = conflict.current.revision
                    restore(sent)
                    adopt(conflict.current.settings)
                    conflicts += 1
                    if conflicts == 2 {
                        log.error("Settings update stopped after repeated revision conflicts")
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
                        adopt(fresh.settings)
                    } catch {
                        log.error("Settings recovery failed: \(String(describing: error), privacy: .private)")
                    }
                } else {
                    restore(sent)
                }
                return
            }
        }
    }
}
