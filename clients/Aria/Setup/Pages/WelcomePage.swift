import SwiftUI

/// Says what Aria gives, as Apple's own welcome screens do. Setup's chores
/// wait for Customize….
struct WelcomePage: View {
    @ObservedObject var state: SetupState
    /// One width for every symbol, so the titles line up.
    @ScaledMetric(relativeTo: .body) private var symbolWidth: CGFloat = 22

    var body: some View {
        Form {
            Section {
                benefit(
                    "waveform", "Dictate anywhere", "Tap the Aria key and speak. Aria types where your cursor is.")
                benefit("magnifyingglass", "Open Aria from any app", "Search commands, prompts, and history.")
                benefit("bubble.left.and.text.bubble.right", "Ask your assistant", "Speak or type a question.")
            } header: {
                hero
            } footer: {
                VStack(alignment: .leading, spacing: SetupMetrics.footerSpacing) {
                    if state.step == .welcome {
                        let plan = SetupState.expressPlan(state.inspection, state.selections)
                        DownloadFooter(total: SetupState.total(plan), notes: state.selfFetchingNotes)
                    }
                    if let missing = state.missingSummary {
                        Text(missing)
                    }
                    NoticeText(notice: state.blockedBy)
                }
            }
        }
    }

    private var hero: some View {
        VStack(spacing: SetupMetrics.captionSpacing) {
            AppIcon(side: SettingsMetrics.heroIconSide)
            Text(state.step.title)
                .font(.largeTitle.bold())
                .foregroundStyle(.primary)
                .accessibilityAddTraits(.isHeader)
            if let caption = state.step.caption {
                Text(caption)
                    .font(.body)
                    .foregroundStyle(.secondary)
            }
        }
        .multilineTextAlignment(.center)
        .frame(maxWidth: .infinity)
        .padding(.bottom, SetupMetrics.pictureInset)
    }

    private func benefit(_ symbol: String, _ title: String, _ detail: String) -> some View {
        Label {
            Text(title)
            Text(detail)
        } icon: {
            Image(systemName: symbol)
                .foregroundStyle(.tint)
                .frame(width: symbolWidth)
        }
    }
}
