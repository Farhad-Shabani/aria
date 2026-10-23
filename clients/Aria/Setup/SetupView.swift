import SwiftUI

struct SetupView: View {
    /// Where the grouped form draws its boxes, so the buttons line up with them.
    private static let boxInset: CGFloat = 20
    /// Where the form's rows and section headers start, so the title lines up with them.
    private static let textInset: CGFloat = 30
    /// Keeps the header and buttons clear of a page that scrolls under them.
    private static let scrollGap: CGFloat = 12

    @ObservedObject var state: SetupState
    @ObservedObject var preferences: DaemonPreferences
    @Environment(\.dismiss) private var dismiss
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        SidebarWindow {
            SetupRail(progress: state.progress)
        } detail: {
            VStack(alignment: .leading, spacing: 0) {
                header
                // The outgoing and incoming pages overlap here instead of
                // sharing the column; the id follows the page, not the step.
                ZStack(alignment: .topLeading) {
                    page
                        .id(state.step.order)
                        .transition(transition)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                .clipped()
                footer
            }
        }
        .animation(.easeInOut(duration: 0.25), value: state.step)
        .onAppear { state.start() }
        .onDisappear { state.stop() }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(state.step.title).font(.title2.bold())
            if let caption = state.step.caption {
                Text(caption).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, Self.textInset)
        .padding(.bottom, Self.scrollGap)
    }

    @ViewBuilder private var page: some View {
        switch state.step {
        case .inspecting, .welcome: WelcomePage(state: state)
        case .choices: EnginesPage(state: state)
        case .applying: InstallingPage(state: state)
        case .grant: GrantPage()
        case .voice: VoicePage(state: state)
        case .tryOut: TryOutPage(state: state)
        case .done: DonePage(state: state, dictationKey: dictationKey, askHotkey: askHotkey)
        case .failed(let message): FailedPage(message: message)
        }
    }

    /// The daemon's default until it answers.
    private var dictationKey: HotkeyKey {
        preferences.snapshot?.storedDictationHotkey ?? .default
    }

    private var askHotkey: AskHotkey? {
        preferences.snapshot?.storedAskHotkey.flatMap { $0.isActive ? $0 : nil }
    }

    private var footer: some View {
        HStack {
            Spacer()
            buttons
        }
        .padding(.top, Self.scrollGap)
        .padding([.horizontal, .bottom], Self.boxInset)
    }

    @ViewBuilder private var buttons: some View {
        switch state.step {
        case .inspecting, .welcome:
            if state.step == .inspecting {
                ProgressView().controlSize(.small)
                Text("Checking this Mac…").foregroundStyle(.secondary)
            }
            Button("Customize…") { state.next() }
                .disabled(state.step == .inspecting)
            Button("Express Setup") { state.express() }
                .primaryAction()
                .disabled(state.step == .inspecting || state.blockedBy != nil)
        case .choices:
            Button("Back") { state.back() }
            Button("Set Up") { state.next() }
                .primaryAction()
                .disabled(state.blockedBy != nil)
        case .applying:
            // Only a download can be cut short cleanly; the other steps
            // are too quick to catch, and a half-installed service is not
            // something to leave behind.
            Button("Cancel") { state.cancel() }.disabled(state.applying != .models)
        case .grant:
            Button("Skip") { state.next() }
            Button("Open System Settings") { state.openAccessibility() }
                .primaryAction()
        case .voice:
            Button("Skip") { state.next() }.disabled(state.busy)
            Button("Record") { state.recordVoice() }
                .primaryAction()
                .disabled(state.busy || state.voiceName.trimmingCharacters(in: .whitespaces).isEmpty)
        case .tryOut:
            Button(state.heard == nil ? "Skip" : "Finish") { state.next() }.disabled(state.busy)
            Button("Record") { state.tryPress() }
                .primaryAction()
                .disabled(state.busy)
        case .done:
            Button("Close") { dismiss() }
                .primaryAction()
        case .failed:
            Button("Close") { dismiss() }
            Button("Try Again") { state.retry() }
                .primaryAction()
        }
    }

    private var transition: AnyTransition {
        if reduceMotion { return .opacity }
        return .asymmetric(
            insertion: .move(edge: state.forward ? .trailing : .leading),
            removal: .move(edge: state.forward ? .leading : .trailing)
        )
    }
}

extension View {
    /// The one button a page answers Return with.
    fileprivate func primaryAction() -> some View {
        buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
    }
}
