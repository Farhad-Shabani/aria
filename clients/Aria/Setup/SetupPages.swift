import SwiftUI

struct WelcomePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        HStack(alignment: .top, spacing: 24) {
            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable()
                .frame(width: 96, height: 96)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 10) {
                Label("Pick speech and chat engines", systemImage: "waveform")
                Label("Download the models they need", systemImage: "arrow.down.circle")
                Label("Run Aria in the background", systemImage: "gearshape")
                if let missing = state.missingSummary {
                    Text(missing).foregroundStyle(.secondary).padding(.top, 8)
                }
                if let blocked = state.blockedBy {
                    Text(blocked).foregroundStyle(.red)
                }
            }
        }
        .padding(.top, 8)
    }
}

struct EnginesPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            LabeledContent("Speech to text:") { picker("Speech to text", state.inspection.stt, $state.selections.stt) }
            LabeledContent("Text to speech:") { picker("Text to speech", state.inspection.tts, $state.selections.tts) }
            LabeledContent("Chat agent:") { picker("Chat agent", state.inspection.chat, $state.selections.chat) }
            LabeledContent("Wake word:") {
                Toggle("Listen for the wake word", isOn: $state.selections.wake)
            }
            .settingsGroup()
            if state.inspection.macos {
                LabeledContent("Hotkey:") {
                    VStack(alignment: .leading, spacing: 4) {
                        Toggle("Start dictation with a hotkey", isOn: $state.selections.hotkey)
                        Text("Needs the Accessibility grant; the next pages walk you through it.")
                            .settingsFootnote()
                    }
                }
            }
        }
        .formStyle(.columns)
    }

    /// The rows carry only labels; the chosen engine's note goes under the
    /// menu, where a path-long note has room.
    private func picker(_ title: String, _ engines: [Aria_Setup_V1_Engine], _ key: Binding<String>) -> some View {
        let chosen = engines.first { $0.key == key.wrappedValue }
        return VStack(alignment: .leading, spacing: 4) {
            Picker(title, selection: key) {
                ForEach(engines, id: \.key) { engine in
                    Text(engine.label).tag(engine.key)
                }
                // config.yaml may name an engine setup does not offer; the
                // picker still needs a row for it.
                if chosen == nil {
                    Text("\(key.wrappedValue) (from config.yaml)").tag(key.wrappedValue)
                }
            }
            .labelsHidden()
            if let chosen, !chosen.note.isEmpty {
                Text(chosen.note)
                    .font(.callout)
                    .foregroundStyle(chosen.availability == .unsupported ? Color.red : Color.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

struct ModelsPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        let plan = SetupState.plan(state.inspection, state.selections)
        VStack(alignment: .leading, spacing: 12) {
            if plan.isEmpty {
                ContentUnavailableView(
                    "Nothing to download", systemImage: "checkmark.circle",
                    description: Text("Every model the chosen engines need is already on disk.")
                )
            } else {
                ForEach(plan, id: \.name) { model in
                    VStack(alignment: .leading, spacing: 2) {
                        Toggle(model.title, isOn: toggle(model.name))
                        Text(Self.detail(model)).settingsFootnote()
                    }
                }
                Text(state.downloadSummary).foregroundStyle(.secondary).padding(.top, 4)
            }
        }
    }

    private func toggle(_ name: String) -> Binding<Bool> {
        Binding(
            get: { state.downloads.contains(name) },
            set: { on in
                if on { state.downloads.insert(name) } else { state.downloads.remove(name) }
            }
        )
    }

    private static func detail(_ model: Aria_Setup_V1_Model) -> String {
        let size = SetupState.size(model.bytes)
        return model.engine.isEmpty ? size : "for \(model.engine) · \(size)"
    }
}

struct InstallingPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(state.installRows, id: \.step) { row in
                HStack(spacing: 8) {
                    mark(row.progress).frame(width: 16)
                    Text(row.title).foregroundStyle(row.progress == .pending ? .secondary : .primary)
                }
                if row.step == .models {
                    ForEach(state.progress.keys.sorted(), id: \.self) { model in
                        if let progress = state.progress[model] { bar(model, progress) }
                    }
                }
            }
        }
    }

    @ViewBuilder private func mark(_ progress: SetupState.InstallRow.Progress) -> some View {
        switch progress {
        case .pending:
            Image(systemName: "circle").foregroundStyle(.secondary)
        case .running:
            ProgressView().controlSize(.small)
        case .done:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        }
    }

    private func bar(_ model: String, _ progress: Aria_Setup_V1_DownloadProgress) -> some View {
        let received = min(progress.received, progress.total)
        let title = state.inspection.models.first { $0.name == model }?.title ?? model
        return ProgressView(value: Double(received), total: Double(max(progress.total, 1))) {
            Text("\(title) · \(SetupState.size(received)) of \(SetupState.size(progress.total))")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(.leading, 24)
    }
}

struct GrantPage: View {
    var body: some View {
        PageNote(symbol: "lock.shield") {
            Text("In System Settings, open Privacy & Security → Accessibility and switch on Aria.")
            Text("This page moves on by itself once the hotkey arms.")
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Waiting for the grant…").foregroundStyle(.secondary)
            }
            .padding(.top, 8)
        }
    }
}

struct VoicePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        PageNote(symbol: "person.wave.2") {
            Text("Say a few sentences when asked. macOS may ask for the microphone.")
            LabeledContent("Name:") {
                TextField("As transcripts should show it", text: $state.voiceName).frame(width: 240)
            }
            .padding(.top, 4)
            if let prompt = state.voicePrompt { Text(prompt).fontWeight(.medium) }
            if let notice = state.notice { Text(notice).foregroundStyle(.red) }
        }
    }
}

struct TryOutPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        PageNote(symbol: "waveform") {
            Text("Press Record, say something, then stop talking. Aria answers out loud.")
            if let enrolled = state.enrolled { Text(enrolled).foregroundStyle(.secondary) }
            if let heard = state.heard {
                Text("“\(heard)”")
                    .textSelection(.enabled)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            }
            if let notice = state.notice { Text(notice).foregroundStyle(.red) }
        }
    }
}

struct DonePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        PageNote(symbol: "checkmark.seal.fill", tint: .green) {
            if let summary = state.summary {
                ForEach(summary.models, id: \.model) { model in
                    Text("\(model.model): \(Self.outcome(model.outcome))")
                }
                Text("Background service: registered")
                if state.selections.hotkey {
                    Text(
                        summary.grant == .armed ? "Hotkey: armed" : "Hotkey: arms once the Accessibility grant is given"
                    )
                }
            }
        }
    }

    private static func outcome(_ outcome: Aria_Setup_V1_Outcome) -> String {
        switch outcome {
        case .downloaded: "downloaded"
        case .present: "already there"
        case .skipped: "skipped"
        case .unspecified, .UNRECOGNIZED: "unknown"
        }
    }
}

struct FailedPage: View {
    let message: String

    var body: some View {
        ContentUnavailableView {
            Image(systemName: "exclamationmark.triangle").font(.largeTitle)
        } description: {
            Text(message).textSelection(.enabled)
        }
    }
}

/// A big symbol beside a short column of text, the shape of every page
/// that only reports or waits.
private struct PageNote<Content: View>: View {
    let symbol: String
    var tint: Color = .accentColor
    @ViewBuilder let content: Content

    var body: some View {
        HStack(alignment: .top, spacing: 20) {
            Image(systemName: symbol)
                .font(.system(size: 40))
                .foregroundStyle(tint)
                .frame(width: 56)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 8) { content }
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.top, 8)
    }
}
