import SwiftUI

/// The pairing code beside the steps to scan it, as the Phone pane and the palette show it.
struct PairingCodeCard<Actions: View>: View {
    private static var pairedNoticeLifetime: TimeInterval { 10 }
    private static var stepNumberSide: CGFloat { 18 }
    private static var stepSpacing: CGFloat { 10 }
    private static var stepTextInset: CGFloat { stepNumberSide + stepSpacing }

    let invite: PairingInvite
    let expiresAt: Date
    let phonePairedAt: Date?
    @ViewBuilder let actions: Actions

    var body: some View {
        HStack(spacing: 24) {
            PairingCodeImage(invite: invite)
            VStack(alignment: .leading, spacing: 0) {
                Spacer(minLength: 0)
                steps
                Spacer(minLength: 0)
                actions
            }
            .frame(height: PairingCodeImage.side)
        }
    }

    private var steps: some View {
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
                if let phonePairedAt, context.date < phonePairedAt.addingTimeInterval(Self.pairedNoticeLifetime) {
                    Text("A phone paired just now.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .padding(.leading, Self.stepTextInset)
                }
            }
        }
    }

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

extension PairingCodeCard where Actions == EmptyView {
    init(invite: PairingInvite, expiresAt: Date, phonePairedAt: Date?) {
        self.init(invite: invite, expiresAt: expiresAt, phonePairedAt: phonePairedAt) { EmptyView() }
    }
}
