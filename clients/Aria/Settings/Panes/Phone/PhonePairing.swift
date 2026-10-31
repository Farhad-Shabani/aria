import AppKit
import GRPCCore
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "PhonePairing")

@MainActor
final class PhonePairing: ObservableObject {
    nonisolated private static let pollInterval: Duration = .seconds(2)

    /// Mirrors CODE_LIFETIME in crates/aria/src/daemon/relay/code.rs.
    static let codeLifetime: TimeInterval = 60

    /// Near expiry the daemon can still serve the old code for a moment, so
    /// polls never come closer together than this.
    nonisolated private static let minimumPoll: Duration = .milliseconds(250)

    /// The expiry is estimated after the fetch returns, so it runs a little
    /// late. A code replaced with more than this left was redeemed, not expired.
    nonisolated private static let expiryTolerance: TimeInterval = 1

    /// Asks clipboard managers not to record the code.
    private static let concealedPasteboardType = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")

    enum Status: Equatable {
        case loading
        case relayOff
        case daemonOff
        case failed
        case live(PairingInvite, expiresAt: Date)
        case restarting
    }

    @Published private(set) var status: Status = .loading
    @Published private(set) var phonePairedAt: Date?
    /// The switch position shown while Aria restarts. Nil otherwise.
    @Published private(set) var targetIsOn: Bool?
    @Published private(set) var actionError: String?

    private let poller: StatusPoller
    private let daemon: PairingDaemon

    init(poller: StatusPoller, daemon: PairingDaemon = .live) {
        self.poller = poller
        self.daemon = daemon
    }

    private var isLive: Bool {
        if case .live = status { return true }
        return false
    }

    var isOn: Bool {
        targetIsOn ?? isLive
    }

    var canSwitch: Bool {
        targetIsOn == nil && poller.state != .working && (isLive || status == .relayOff)
    }

    var canRevoke: Bool {
        targetIsOn == nil && poller.state != .working
    }

    func run() async {
        while !Task.isCancelled {
            // A restart in progress would read as the daemon being off.
            let next = status == .restarting ? Self.pollInterval : await poll()
            try? await Task.sleep(for: next)
        }
    }

    @discardableResult
    func poll() async -> Duration {
        let issued: (invite: PairingInvite, expiresAt: Date)
        do {
            issued = try await daemon.issueInvite()
        } catch {
            let failure = Self.status(for: error)
            guard status != .restarting else { return Self.pollInterval }
            // Polled every couple of seconds, so only a new failure is logged.
            if case .failed = failure, failure != status {
                log.error("Fetching a pairing code failed: \(String(describing: error), privacy: .private)")
            }
            status = failure
            return Self.pollInterval
        }
        // A poll in flight when a restart began must not bring the code back.
        if status == .restarting { return Self.pollInterval }
        if case .live(let shown, let expiresAt) = status {
            if shown == issued.invite { return Self.nextPoll(expiresAt: expiresAt) }
            if Self.wasRedeemed(expiresAt: expiresAt, replacedAt: .now) {
                log.notice("A phone redeemed the pairing code")
                phonePairedAt = .now
            }
        }
        status = .live(issued.invite, expiresAt: issued.expiresAt)
        return Self.nextPoll(expiresAt: issued.expiresAt)
    }

    /// The daemon registers IssueInvite only while the relay runs, so
    /// unimplemented means the relay is off. Unavailable means the daemon is
    /// not listening at all. Any other error's detail is logged, not shown.
    nonisolated static func status(for error: any Error) -> Status {
        switch (error as? RPCError)?.code {
        case .unimplemented: return .relayOff
        case .unavailable: return .daemonOff
        default: return .failed
        }
    }

    nonisolated static func nextPoll(expiresAt: Date, now: Date = .now) -> Duration {
        let untilExpiry = Duration.seconds(expiresAt.timeIntervalSince(now))
        return max(minimumPoll, min(pollInterval, untilExpiry))
    }

    nonisolated static func wasRedeemed(expiresAt: Date, replacedAt now: Date) -> Bool {
        now.addingTimeInterval(expiryTolerance) < expiresAt
    }

    /// The daemon reads the pairing setting only at start, so the restart is
    /// what applies it.
    func turn(on: Bool) async {
        targetIsOn = on
        defer { targetIsOn = nil }
        // The code goes at once, so nobody scans it while pairing turns off.
        status = .restarting
        do {
            try await daemon.setPairing(on)
        } catch {
            log.error(
                "Turning phone pairing \(on ? "on" : "off", privacy: .public) failed: \(error.localizedDescription, privacy: .private)"
            )
            actionError = "Aria couldn’t turn phone pairing \(on ? "on" : "off"). Try again."
            status = .loading
            await poll()
            return
        }
        actionError = nil
        await restart()
        // The daemon also reports the relay as off when pairing is on but its
        // port or pairing file is unusable.
        if on, status == .relayOff {
            log.error("Phone pairing is on but the relay did not start")
            actionError = "Phone pairing is on but couldn’t start. Run `aria doctor` in Terminal to see why."
        }
    }

    /// Deleting the pairing file while Aria is stopped makes the daemon mint a
    /// new certificate on start, which cuts off every paired phone.
    func revoke() async {
        targetIsOn = true
        defer { targetIsOn = nil }
        status = .restarting
        do {
            try await poller.restart(whileStopped: daemon.removePairingFile)
            actionError = nil
        } catch let failure as StatusPoller.RestartFailure {
            log.error("Revoking phones failed: \(failure.localizedDescription, privacy: .private)")
            actionError = "Aria couldn’t revoke the phones. \(failure.localizedDescription)"
        } catch {
            log.error("Revoking phones failed: \(error.localizedDescription, privacy: .private)")
            actionError = "Aria couldn’t revoke the phones. Try again."
        }
        status = .loading
        await poll()
    }

    /// The poller refuses a restart while it is already starting or stopping
    /// Aria. The setting is saved by then, so it applies on the next start.
    private func restart() async {
        status = .restarting
        do {
            try await poller.restart()
        } catch StatusPoller.RestartFailure.busy {
            log.notice("Restart deferred: Aria is busy")
            actionError = "Aria is busy. The change applies when it next starts."
        } catch {
            log.error("Restart failed: \(error.localizedDescription, privacy: .private)")
            actionError =
                (error as? StatusPoller.RestartFailure)?.localizedDescription ?? "Something went wrong. Try again."
        }
        status = .loading
        await poll()
    }

    /// The general pasteboard is the only one the phone's paste field can
    /// reach, and it syncs through Universal Clipboard. The concealed marker
    /// is a hint clipboard managers may ignore, so the code is also wiped
    /// when it expires, unless something else was copied since.
    func copy(_ invite: PairingInvite, until wipeAt: Date) {
        guard let text = invite.url?.absoluteString else { return }
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        pasteboard.setString("", forType: Self.concealedPasteboardType)
        pasteboard.setString(text, forType: .string)
        let copied = pasteboard.changeCount
        Task {
            try? await Task.sleep(for: .seconds(max(0, wipeAt.timeIntervalSinceNow)))
            if pasteboard.changeCount == copied {
                pasteboard.clearContents()
            }
        }
    }
}
