import CoreImage.CIFilterBuiltins
import GRPCCore
import GRPCNIOTransportHTTP2
import SwiftUI
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "PhoneSettings")

/// Pairs an iPhone: shows the one-time code the phone scans, or why there
/// is none.
struct PhoneSettings: View {
    /// Picks up a code a phone used. Expiry is polled for exactly, so the
    /// next code appears the moment this one lapses.
    nonisolated private static let reissueInterval: Duration = .seconds(2)

    private static let pairedNoticeLifetime: TimeInterval = 10

    /// Mirrors CODE_LIFETIME in crates/aria/src/daemon/relay/code.rs.
    private static let codeLifetime: TimeInterval = 60

    /// The shortest wait between fetches, for a daemon a beat behind on rotating.
    nonisolated private static let minimumPoll: Duration = .milliseconds(250)

    /// How much of a code's life can lapse between polls. A code replaced
    /// with more left than this was redeemed, not expired.
    nonisolated private static let expiryTolerance: TimeInterval = 1

    /// Asks clipboard managers not to record the code.
    private static let concealedPasteboardType = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")

    enum Status: Equatable {
        case loading
        /// The daemon answers but serves no pairing codes: the relay is off.
        case relayOff
        case daemonOff
        case failed(String)
        case live(PairingInvite, expiresAt: Date)
        /// Aria is restarting, so the daemon is briefly off.
        case restarting
    }

    @ObservedObject var poller: StatusPoller
    @State private var status: Status = .loading
    @State private var phonePairedAt: Date?
    /// What the switch shows while Aria restarts to apply it.
    @State private var pairingTarget: Bool?
    @State private var actionError: String?
    @State private var confirmsRevoke = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            switch status {
            case .loading, .restarting, .relayOff:
                EmptyView()
            case .daemonOff:
                daemonOff.padding(.bottom, 16)
            case .failed(let message):
                Text(message)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.bottom, 16)
            case .live(let invite, let expiresAt):
                Divider()
                live(invite, expiresAt: expiresAt)
            }
            if let error = actionError {
                Text(error)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.bottom, 16)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 20)
        .task {
            while !Task.isCancelled {
                // A restart in progress would read as the daemon being off.
                let next = status == .restarting ? Self.reissueInterval : await load()
                try? await Task.sleep(for: next)
            }
        }
    }

    /// Returns the wait before the next fetch. The invite is replaced only
    /// when the code changes, so the QR is not redrawn every tick.
    private func load() async -> Duration {
        let issued: (invite: PairingInvite, expiresAt: Date)
        do {
            issued = try await Self.issueInvite()
        } catch {
            let failure = Self.status(for: error)
            guard status != .restarting else { return Self.reissueInterval }
            // Polled every couple of seconds, so only a new failure is logged.
            if case .failed = failure, failure != status {
                log.error("Fetching a pairing code failed: \(String(describing: error), privacy: .private)")
            }
            status = failure
            return Self.reissueInterval
        }
        // A poll in flight when a restart began must not bring the code back.
        if status == .restarting { return Self.reissueInterval }
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

    /// The daemon serves IssueInvite only while the relay runs; a daemon that
    /// is not listening at all is a different failure.
    nonisolated static func status(for error: any Error) -> Status {
        switch (error as? RPCError)?.code {
        case .unimplemented: return .relayOff
        case .unavailable: return .daemonOff
        default: return .failed(error.localizedDescription)
        }
    }

    nonisolated static func nextPoll(expiresAt: Date, now: Date = .now) -> Duration {
        let untilExpiry = Duration.seconds(expiresAt.timeIntervalSince(now))
        return max(minimumPoll, min(reissueInterval, untilExpiry))
    }

    nonisolated static func wasRedeemed(expiresAt: Date, replacedAt now: Date) -> Bool {
        now.addingTimeInterval(expiryTolerance) < expiresAt
    }

    private static func issueInvite() async throws -> (invite: PairingInvite, expiresAt: Date) {
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            let invites = Aria_Daemon_V1_InviteService.Client(wrapping: client)
            return try await invites.issueInvite(Aria_Daemon_V1_IssueInviteRequest()) { response in
                let issued = try response.message
                let invite = PairingInvite(
                    instanceName: issued.instance, code: issued.code,
                    fingerprint: issued.fingerprint)
                return (invite, Date.now.addingTimeInterval(TimeInterval(issued.expiresInMs) / 1000))
            }
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Phone Pairing").fontWeight(.semibold)
            Spacer()
            if status == .restarting {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Restarting Aria…")
                }
                .font(.callout)
                .foregroundStyle(.secondary)
            }
            Toggle(
                "Phone Pairing",
                isOn: Binding(
                    get: { pairingTarget ?? isLive },
                    set: { on in Task { await setPhonePairing(on) } }
                )
            )
            .toggleStyle(.switch)
            .controlSize(.small)
            .labelsHidden()
            .disabled(!canSwitch)
        }
        .frame(height: 48)
    }

    private var isLive: Bool {
        if case .live = status { return true }
        return false
    }

    private var canSwitch: Bool {
        pairingTarget == nil && poller.state != .working && (isLive || status == .relayOff)
    }

    private var daemonOff: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Aria isn't running.")
            if let error = poller.lastError {
                Text(error).settingsFootnote()
            }
        }
    }

    private func live(_ invite: PairingInvite, expiresAt: Date) -> some View {
        HStack(spacing: 24) {
            CodeImage(invite: invite)
            VStack(alignment: .leading, spacing: 0) {
                Spacer(minLength: 0)
                steps(expiresAt: expiresAt)
                Spacer(minLength: 0)
                actions(invite, expiresAt: expiresAt)
            }
            .frame(width: 248, height: CodeImage.side)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 24)
        .confirmationDialog("Revoke every paired phone?", isPresented: $confirmsRevoke) {
            Button("Revoke and Restart Aria", role: .destructive) {
                Task { await revoke() }
            }
        } message: {
            Text("Aria restarts with a new code. Each phone scans it again.")
        }
    }

    private func steps(expiresAt: Date) -> some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let secondsLeft = max(0, expiresAt.timeIntervalSince(context.date).rounded(.up))
            let left = Duration.seconds(secondsLeft).formatted(.time(pattern: .minuteSecond))
            VStack(alignment: .leading, spacing: 12) {
                step(1) { openPhoneApp }
                step(2, "Tap **Pair with Mac**.")
                step(3) {
                    HStack(spacing: 6) {
                        Text("Scan this code.")
                        CountdownRing(fraction: min(1, secondsLeft / Self.codeLifetime))
                            .help("Code refreshes in \(left)")
                            .accessibilityLabel("Code refreshes in \(left)")
                    }
                }
                if let phonePairedAt,
                    context.date < phonePairedAt.addingTimeInterval(Self.pairedNoticeLifetime)
                {
                    Text("A phone paired just now.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .padding(.leading, Self.stepTextInset)
                }
            }
        }
    }

    private func actions(_ invite: PairingInvite, expiresAt: Date) -> some View {
        HStack(spacing: 8) {
            Button {
                guard let text = invite.url?.absoluteString else { return }
                copy(text, until: expiresAt)
            } label: {
                Text("Copy Code").frame(maxWidth: .infinity)
            }
            Button(role: .destructive) {
                confirmsRevoke = true
            } label: {
                Text("Revoke All…").frame(maxWidth: .infinity)
            }
            .foregroundStyle(.red)
            .disabled(pairingTarget != nil || poller.state == .working)
        }
    }

    private static let stepNumberSide: CGFloat = 18
    private static let stepSpacing: CGFloat = 10
    private static let stepTextInset = stepNumberSide + stepSpacing

    private func step(_ number: Int, _ text: LocalizedStringKey) -> some View {
        step(number) { Text(text) }
    }

    private func step(_ number: Int, @ViewBuilder _ content: () -> some View) -> some View {
        HStack(spacing: Self.stepSpacing) {
            Text("\(number)")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: Self.stepNumberSide, height: Self.stepNumberSide)
                .overlay(Circle().strokeBorder(.tertiary))
            content()
        }
    }

    @ViewBuilder
    private var openPhoneApp: some View {
        if let url = PhoneApp.appStoreURL {
            Text("Open \(Self.link("Aria for iPhone", to: url)).")
        } else {
            HStack(spacing: 6) {
                Text("Open Aria for iPhone.")
                Text("Soon")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 4)
                    .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(.tertiary))
            }
        }
    }

    private static func link(_ text: String, to url: URL) -> AttributedString {
        var link = AttributedString(text)
        link.link = url
        return link
    }

    /// The daemon reads the pairing setting only at start, so the restart is
    /// what applies it.
    private func setPhonePairing(_ on: Bool) async {
        pairingTarget = on
        defer { pairingTarget = nil }
        // The code goes at once, so nobody scans it while pairing turns off.
        status = .restarting
        do {
            try await Task.detached { try DaemonClient.setPhonePairing(on) }.value
        } catch {
            log.error(
                "Turning phone pairing \(on ? "on" : "off", privacy: .public) failed: \(error.localizedDescription, privacy: .private)"
            )
            actionError = "Phone pairing could not be turned \(on ? "on" : "off"): \(error.localizedDescription)"
            status = .loading
            _ = await load()
            return
        }
        actionError = nil
        await restart()
        // The daemon answers the same way when pairing is on but its port
        // or pairing file is unusable.
        if on, status == .relayOff {
            log.error("Phone pairing is on but the relay did not start")
            actionError = "Phone pairing is on but couldn't start. Run aria doctor in Terminal to see why."
        }
    }

    /// The daemon mints new material on start, so the restart is the
    /// revocation.
    private func revoke() async {
        pairingTarget = true
        defer { pairingTarget = nil }
        status = .restarting
        do {
            try await poller.restart(whileStopped: { try PairingFile.remove() })
            actionError = nil
        } catch {
            log.error("Revoking phones failed: \(error.localizedDescription, privacy: .private)")
            actionError = "Phones could not be revoked: \(error.localizedDescription)"
        }
        status = .loading
        _ = await load()
    }

    /// The poller refuses a restart while it is already starting or stopping
    /// Aria from the menu bar, so the change waits for the next start.
    private func restart() async {
        status = .restarting
        do {
            try await poller.restart()
        } catch StatusPoller.RestartFailure.busy {
            log.notice("Restart deferred: Aria is busy")
            actionError = "Aria is busy. The change applies when it next starts."
        } catch {
            log.error("Restart failed: \(error.localizedDescription, privacy: .private)")
            actionError = error.localizedDescription
        }
        status = .loading
        _ = await load()
    }

    /// The general pasteboard is the only one the phone's paste field can
    /// reach, and it syncs through Universal Clipboard. The concealed marker
    /// is a hint clipboard managers may ignore, so the code is also wiped
    /// at `wipeAt`, unless something else was copied since.
    private func copy(_ text: String, until wipeAt: Date) {
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

/// The QR image. Equatable so SwiftUI leaves it alone while the code is
/// the same and only the countdown beside it ticks.
private struct CodeImage: View, Equatable {
    /// The white tile's side, quiet zone included.
    static let side: CGFloat = 160
    private static let quietZone: CGFloat = 12

    let invite: PairingInvite

    var body: some View {
        if let code = qrCode {
            // White in dark mode too, so the phone's camera still reads it.
            Image(nsImage: code)
                .interpolation(.none)
                .resizable()
                .frame(width: Self.side - 2 * Self.quietZone, height: Self.side - 2 * Self.quietZone)
                .padding(Self.quietZone)
                .background(.white, in: RoundedRectangle(cornerRadius: 12))
                .accessibilityLabel("Pairing code")
        }
    }

    private var qrCode: NSImage? {
        guard let text = invite.url?.absoluteString else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "L"
        guard let image = filter.outputImage else { return nil }
        let representation = NSCIImageRep(ciImage: image)
        let result = NSImage(size: representation.size)
        result.addRepresentation(representation)
        return result
    }
}

private struct CountdownRing: View {
    let fraction: Double

    var body: some View {
        ZStack {
            Circle().stroke(.quaternary, lineWidth: 2)
            Circle()
                .trim(from: 0, to: fraction)
                .stroke(.secondary, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: 12, height: 12)
        .animation(.linear(duration: 1), value: fraction)
    }
}
