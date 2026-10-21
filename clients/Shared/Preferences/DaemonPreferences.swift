import Foundation
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "DaemonPreferences")

/// Synchronizes daemon preferences through one revision and pending patch.
/// The transport decides which daemon and over what; this class does not.
///
/// The Mac passes a UserDefaults key so its overlay keeps the captions choice
/// while the daemon is unavailable; the phone passes none.
@MainActor
final class DaemonPreferences: ObservableObject {
    @Published private(set) var captionsEnabled: Bool
    /// Nil until the daemon answers.
    @Published private(set) var snapshot: Aria_Daemon_V1_UserPreferences?
    /// The last failed sync or push; cleared by the next one that succeeds.
    @Published private(set) var lastError: (any Error)?

    private let transport: any PreferencesTransport
    private let captionsDefaultsKey: String?
    private var revision: UInt64?
    /// Changes the daemon has not accepted yet; the next sync pushes them.
    private var pending = Aria_Daemon_V1_PreferencePatch()
    private var pushing = false

    init(transport: any PreferencesTransport, captionsDefaultsKey: String? = nil) {
        self.transport = transport
        self.captionsDefaultsKey = captionsDefaultsKey
        captionsEnabled = captionsDefaultsKey.map { UserDefaults.standard.bool(forKey: $0) } ?? false
    }

    var historyEnabled: Bool {
        snapshot?.dictationHistory ?? true
    }

    var deliveryLocked: Bool {
        snapshot.map { DictationEdits.locksDelivery($0.outputMode) } ?? false
    }

    func sync() async {
        do {
            let snapshot = try await transport.snapshot()
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
            log.error("Sync failed: \(String(describing: error), privacy: .private)")
            lastError = error
        }
    }

    /// Shows the edit at once and pushes it; the daemon's answer replaces it.
    func set(_ edit: (inout Aria_Daemon_V1_PreferencePatch) -> Void) async {
        edit(&pending)
        if var shown = snapshot {
            pending.apply(to: &shown)
            show(shown)
        } else if pending.hasDeliveryMode {
            // Offline, the captions choice must still move the overlay.
            showCaptions(pending.deliveryMode == .complete)
        }
        await push()
    }

    /// Lays unaccepted edits over the daemon's snapshot, so a reply to an
    /// older push never flips a control the user has since moved.
    private func adopt(_ accepted: Aria_Daemon_V1_UserPreferences) {
        var shown = accepted
        pending.apply(to: &shown)
        show(shown)
    }

    private func show(_ preferences: Aria_Daemon_V1_UserPreferences) {
        snapshot = preferences
        showCaptions(preferences.deliveryMode == .complete)
    }

    private func showCaptions(_ on: Bool) {
        captionsEnabled = on
        if let captionsDefaultsKey {
            UserDefaults.standard.set(on, forKey: captionsDefaultsKey)
        }
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

    /// Serializes pushes and adopts the accepted snapshot, including fields
    /// the daemon did not apply.
    private func push() async {
        guard !pushing else { return }
        pushing = true
        defer { pushing = false }
        while pending != Aria_Daemon_V1_PreferencePatch() {
            let sent = pending
            for _ in 0..<2 {
                do {
                    if revision == nil {
                        revision = try await transport.snapshot().revision
                    }
                    var request = Aria_Daemon_V1_UpdatePreferencesRequest()
                    request.expectedRevision = revision ?? 0
                    request.patch = sent
                    switch try await transport.update(request).result {
                    case .snapshot(let accepted):
                        revision = accepted.revision
                        lastError = nil
                        if pending == sent {
                            pending = Aria_Daemon_V1_PreferencePatch()
                        }
                        adopt(accepted.preferences)
                    case .conflict(let conflict):
                        revision = conflict.current.revision
                        continue
                    case nil:
                        return
                    }
                } catch {
                    log.error("Update failed: \(String(describing: error), privacy: .private)")
                    lastError = error
                    if Self.refusesPatch(error) {
                        // The daemon will never take this patch; keeping it
                        // would poison every later edit. Show its truth again.
                        pending = Aria_Daemon_V1_PreferencePatch()
                        if let fresh = try? await transport.snapshot() {
                            revision = fresh.revision
                            adopt(fresh.preferences)
                        }
                    }
                    return
                }
                break
            }
        }
    }
}
