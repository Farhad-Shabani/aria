import SwiftUI

struct PhoneSettings: View {
    private static let pairedNoticeLifetime: TimeInterval = 10

    @ObservedObject var poller: StatusPoller
    @StateObject private var pairing: PhonePairing
    @State private var confirmsRevoke = false

    init(poller: StatusPoller) {
        self.poller = poller
        _pairing = StateObject(wrappedValue: PhonePairing(poller: poller))
    }

    var body: some View {
        Group {
            if pairing.status == .daemonOff {
                DaemonWaitingView()
            } else {
                form
            }
        }
        .task { await pairing.run() }
    }

    private var form: some View {
        Form {
            Section {
                Toggle(
                    isOn: Binding(
                        get: { pairing.isOn },
                        set: { on in Task { await pairing.turn(on: on) } }
                    )
                ) {
                    Text("Phone pairing")
                    if pairing.status == .restarting {
                        Text("Restarting Aria…")
                    }
                }
                .disabled(!pairing.canSwitch)
            } footer: {
                problems
            }
            if case .live(let invite, let expiresAt) = pairing.status {
                Section {
                    live(invite, expiresAt: expiresAt)
                }
            }
        }
    }

    @ViewBuilder
    private var problems: some View {
        switch pairing.status {
        case .failed:
            Text("Aria couldn't make a pairing code. Run aria doctor in Terminal to see why.").foregroundStyle(.red)
        case .loading, .restarting, .relayOff, .daemonOff, .live:
            EmptyView()
        }
        if let error = pairing.actionError {
            Text(error).foregroundStyle(.red)
        }
    }

    private func live(_ invite: PairingInvite, expiresAt: Date) -> some View {
        HStack(spacing: 24) {
            PairingCodeImage(invite: invite)
            VStack(alignment: .leading, spacing: 0) {
                Spacer(minLength: 0)
                steps(expiresAt: expiresAt)
                Spacer(minLength: 0)
                actions(invite, expiresAt: expiresAt)
            }
            .frame(height: PairingCodeImage.side)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 8)
        .confirmationDialog("Revoke every paired phone?", isPresented: $confirmsRevoke) {
            Button("Revoke and Restart Aria", role: .destructive) {
                Task { await pairing.revoke() }
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
                        CountdownRing(fraction: min(1, secondsLeft / PhonePairing.codeLifetime))
                            .help("Code refreshes in \(left)")
                            .accessibilityLabel("Code refreshes in \(left)")
                    }
                }
                if let phonePairedAt = pairing.phonePairedAt,
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
                pairing.copy(invite, until: expiresAt)
            } label: {
                Text("Copy Code").frame(maxWidth: .infinity)
            }
            Button(role: .destructive) {
                confirmsRevoke = true
            } label: {
                Text("Revoke All…").frame(maxWidth: .infinity)
            }
            .foregroundStyle(.red)
            .disabled(!pairing.canRevoke)
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
                .overlay { Circle().strokeBorder(.tertiary) }
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
                    .overlay { RoundedRectangle(cornerRadius: 4).strokeBorder(.tertiary) }
            }
        }
    }

    private static func link(_ text: String, to url: URL) -> AttributedString {
        var link = AttributedString(text)
        link.link = url
        return link
    }
}
