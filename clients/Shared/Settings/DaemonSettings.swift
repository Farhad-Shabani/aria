import Foundation
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonSettings")

/// Keeps this client's settings in step with the daemon, using a queue of edits.
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

    /// One change the person made. It builds its patch from `base`, this
    /// client's newest copy of the settings when it runs. So a list edit keeps
    /// entries another client added. An edit can run many times, and `base`
    /// may already hold it. So adding an entry that is already there must add
    /// nothing.
    typealias Edit = @MainActor (_ base: Aria_Daemon_V1_Settings) -> Aria_Daemon_V1_SettingsPatch

    /// The daemon's settings with edits it hasn't accepted applied on top. Nil
    /// until the daemon first answers.
    @Published private(set) var snapshot: Aria_Daemon_V1_Settings?
    /// The last failed sync or push; cleared by the next one that succeeds.
    @Published private(set) var lastError: (any Error)?

    /// How many conflicts in a row one edit can hit before Aria stops
    /// retrying and tells the person. By then another writer is probably still
    /// saving, so more retries won't help.
    private static let conflictLimit = 2

    private let transport: any SettingsTransport
    /// The last snapshot the daemon returned.
    private var answer: Aria_Daemon_V1_SettingsSnapshot?
    /// Edits the daemon has not accepted yet, oldest first. Each goes in its
    /// own request, so a refusal drops only that edit.
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

    /// Shows the edit right away, then sends it. The daemon's answer replaces
    /// what was shown.
    func edit(_ edit: @escaping Edit) async {
        pending.append(edit)
        show()
        await push()
    }

    private func adopt(_ answer: Aria_Daemon_V1_SettingsSnapshot) {
        self.answer = answer
        show()
    }

    /// Applies the queued edits on top of the daemon's answer, so a reply to an
    /// older push never flips a control the user has since moved.
    private func show() {
        guard var shown = answer?.settings else { return }
        for edit in pending {
            edit(shown).apply(to: &shown)
        }
        snapshot = shown
    }

    /// True when the daemon rejected the edit itself, so retrying won't help.
    /// False for any other error, such as an unreachable daemon.
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
        // A refused edit is dropped, but its error stays after later edits
        // succeed. That error is how the person learns the control snapped
        // back. A later failure replaces it.
        var refusal: (any Error)?
        var conflicts = 0
        while let next = pending.first {
            do {
                switch try await send(next) {
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
                // A push under a window's sync is cancelled when the window
                // closes. The edit stays queued for the next sync.
                guard !Task.isCancelled else { return }
                log.error("Update failed: \(String(describing: error), privacy: .private)")
                guard Self.refusesPatch(error) else {
                    lastError = error
                    return
                }
                // A refused write changed nothing, so the daemon's last answer
                // is still right. Only the view needs updating.
                pending.removeFirst()
                show()
                conflicts = 0
                refusal = error
                lastError = error
            }
        }
        lastError = refusal
    }

    /// Sends one edit, built on the daemon's last answer or a fresh fetch.
    private func send(_ edit: Edit) async throws -> Aria_Daemon_V1_UpdateSettingsResponse.OneOf_Result? {
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
        request.patch = edit(base.settings)
        return try await transport.update(request).result
    }
}
