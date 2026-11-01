import Foundation
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonSettings")

/// Synchronizes daemon settings through one revision and a queue of edits.
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

    /// One change the person made, as a patch against whatever the daemon
    /// holds when it is sent. A list edit reads the list from `base`, so a
    /// retry after a conflict builds on another writer's entries instead of
    /// replacing them. An edit runs again every time the base changes, so
    /// it must be idempotent: adding an entry already there adds nothing.
    typealias Edit = @MainActor (_ base: Aria_Daemon_V1_Settings) -> Aria_Daemon_V1_SettingsPatch

    /// The daemon's settings with the queued edits laid over. Nil until the
    /// daemon answers.
    @Published private(set) var snapshot: Aria_Daemon_V1_Settings?
    /// The last failed sync or push; cleared by the next one that succeeds.
    @Published private(set) var lastError: (any Error)?

    /// Conflicts in a row on one edit that end its retries. The first means
    /// another writer got in first, and the retry rebases. The second means
    /// that writer is still at it, and the person hears about it.
    private static let conflictLimit = 2

    private let transport: any SettingsTransport
    /// The daemon's last answer.
    private var answer: Aria_Daemon_V1_SettingsSnapshot?
    /// Changes the daemon has not accepted yet, oldest first. Each goes in
    /// its own request, so a refusal drops only the edit it names.
    @Published private var pending: [Edit] = []
    @Published private var isPushing = false
    /// A fetch that overlaps a push may predate it. The revision can't tell:
    /// it's a hash of the file's values, so it has no order.
    private var pushCount = 0
    private var fetchCount = 0
    private var newestFetchSeen = 0

    init(transport: any SettingsTransport) {
        self.transport = transport
    }

    /// Edits the daemon has not taken yet, with no push under way to
    /// deliver them.
    var hasUnsavedChange: Bool {
        !pending.isEmpty && !isPushing
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
            let fetched = try await transport.snapshot()
            // A snapshot fetched before a push landed is older than what
            // we already know; adopting it would revert the push.
            guard !isPushing, pushCount == pushesBefore, fetch > newestFetchSeen else { return }
            newestFetchSeen = fetch
            adopt(fetched)
            lastError = nil
            if !pending.isEmpty {
                await push()
            }
        } catch {
            // Closing the window cancels its sync; the next one reports the daemon.
            guard !Task.isCancelled else { return }
            log.error("Sync failed: \(String(describing: error), privacy: .private)")
            lastError = error
        }
    }

    /// An edit whose patch does not depend on what the daemon holds.
    func set(_ edit: (inout Aria_Daemon_V1_SettingsPatch) -> Void) async {
        var patch = Aria_Daemon_V1_SettingsPatch()
        edit(&patch)
        await self.edit { _ in patch }
    }

    /// Shows the edit at once and pushes it; the daemon's answer replaces it.
    func edit(_ edit: @escaping Edit) async {
        pending.append(edit)
        show()
        await push()
    }

    private func adopt(_ answer: Aria_Daemon_V1_SettingsSnapshot) {
        self.answer = answer
        show()
    }

    /// Lays the queued edits over the daemon's answer, so a reply to an
    /// older push never flips a control the user has since moved.
    private func show() {
        guard var shown = answer?.settings else { return }
        for edit in pending {
            edit(shown).apply(to: &shown)
        }
        snapshot = shown
    }

    /// A status that says the daemon will never take this edit as sent, as
    /// opposed to the daemon being away.
    static func refusesPatch(_ error: any Error) -> Bool {
        guard let status = error as? RPCError else { return false }
        switch status.code {
        case .invalidArgument, .permissionDenied, .unauthenticated: return true
        default: return false
        }
    }

    private func push() async {
        guard !isPushing else { return }
        isPushing = true
        pushCount += 1
        defer { isPushing = false }
        // A refused edit is dropped, but its refusal must outlive the edits
        // accepted after it: that is how the person learns the control
        // snapped back. A later failure still replaces it, newest first.
        var refusal: (any Error)?
        var conflicts = 0
        while let next = pending.first {
            do {
                try Task.checkCancellation()
                let base: Aria_Daemon_V1_SettingsSnapshot
                if let answer {
                    base = answer
                } else {
                    base = try await transport.snapshot()
                    adopt(base)
                }
                try Task.checkCancellation()
                var request = Aria_Daemon_V1_UpdateSettingsRequest()
                request.expectedRevision = base.revision
                request.patch = next(base.settings)
                switch try await transport.update(request).result {
                case .snapshot(let answer):
                    pending.removeFirst()
                    adopt(answer)
                    conflicts = 0
                case .conflict(let conflict):
                    adopt(conflict.current)
                    conflicts += 1
                    if conflicts == Self.conflictLimit {
                        log.error("Settings update stopped after repeated revision conflicts")
                        lastError = UpdateError.conflict
                        return
                    }
                case nil:
                    throw UpdateError.invalidResponse
                }
            } catch {
                // Closing the window keeps the edit for the next sync.
                guard !Task.isCancelled else { return }
                log.error("Update failed: \(String(describing: error), privacy: .private)")
                guard Self.refusesPatch(error) else {
                    lastError = error
                    return
                }
                // A refused write changed nothing, so the daemon's last
                // answer still stands and the edit only leaves the view.
                pending.removeFirst()
                show()
                refusal = error
                lastError = error
            }
        }
        lastError = refusal
    }
}
