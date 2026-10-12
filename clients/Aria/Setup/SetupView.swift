import SwiftUI

struct SetupView: View {
    @ObservedObject var state: SetupState
    @Environment(\.dismiss) private var dismiss
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
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
        .padding(20)
        .frame(width: 520, height: 440)
        .animation(.easeInOut(duration: 0.25), value: state.step)
        .onAppear {
            NSApplication.shared.activate()
            state.start()
        }
        .onDisappear { state.stop() }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(state.step.title).font(.title2.weight(.semibold))
            if let caption = state.step.caption {
                Text(caption).font(.callout).foregroundStyle(.secondary)
            }
        }
        .padding(.bottom, 16)
    }

    @ViewBuilder private var page: some View {
        switch state.step {
        case .inspecting, .welcome: WelcomePage(state: state)
        case .choices: EnginesPage(state: state)
        case .models: ModelsPage(state: state)
        case .applying: InstallingPage(state: state)
        case .grant: GrantPage()
        case .voice: VoicePage(state: state)
        case .tryOut: TryOutPage(state: state)
        case .done: DonePage(state: state)
        case .failed(let message): FailedPage(message: message)
        }
    }

    private var footer: some View {
        HStack {
            if let stage = state.step.stage { StageDots(current: stage) }
            Spacer()
            buttons
        }
        .padding(.top, 16)
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
            Button("Continue") { state.next() }
                .primaryAction()
                .disabled(state.blockedBy != nil)
        case .models:
            Button("Back") { state.back() }
            Button("Set Up") { state.next() }
                .primaryAction()
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
    func primaryAction() -> some View {
        buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
    }
}

private struct StageDots: View {
    let current: SetupState.Stage

    var body: some View {
        HStack(spacing: 6) {
            ForEach(SetupState.Stage.allCases, id: \.rawValue) { stage in
                Circle()
                    .fill(stage.rawValue <= current.rawValue ? Color.accentColor : Color.secondary.opacity(0.3))
                    .frame(width: 7, height: 7)
            }
        }
        .accessibilityLabel("Step \(current.rawValue + 1) of \(SetupState.Stage.allCases.count)")
    }
}
