import SwiftUI

struct WelcomePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                Label("Pick speech and chat engines", systemImage: "waveform")
                Label("Download the models they need", systemImage: "arrow.down.circle")
                Label("Run Aria in the background", systemImage: "gearshape")
            } footer: {
                if let missing = state.missingSummary {
                    Text(missing)
                }
                if let blocked = state.blockedBy {
                    Text(blocked).foregroundStyle(.red)
                }
            }
            if state.step == .welcome { expressDownloads }
        }
    }

    private var expressDownloads: some View {
        let plan = SetupState.expressPlan(state.inspection, state.selections)
        return Section {
            ForEach(plan, id: \.name) { model in
                LabeledContent(model.title) {
                    Text(SetupState.downloadSize(of: [model])).monospacedDigit()
                }
            }
        } header: {
            if !plan.isEmpty {
                Text("Downloads")
            }
        } footer: {
            DownloadFooter(total: SetupState.total(plan), notes: state.selfFetchingNotes)
        }
    }
}

struct EnginesPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                picker("Speech to text", state.inspection.stt, $state.selections.stt, sized: true)
                picker("Text to speech", state.inspection.tts, $state.selections.tts, sized: true)
                picker("Chat agent", state.inspection.chat, $state.selections.chat, sized: false)
            }
            Section {
                Toggle("Listen for the wake word", isOn: $state.selections.wake)
                if state.inspection.macos {
                    Toggle(isOn: $state.selections.hotkey) {
                        Text("Start dictation with a hotkey")
                        Text("Needs Accessibility access. Setup asks for it next.")
                    }
                }
            }
            DownloadsSection(state: state)
        }
    }

    /// With `sized`, each row shows its download size for comparing. The
    /// chosen engine's note sits under the title, where a long path fits.
    private func picker(_ title: String, _ engines: [Aria_Setup_V1_Engine], _ key: Binding<String>, sized: Bool)
        -> some View
    {
        let chosen = engines.first { $0.key == key.wrappedValue }
        return Picker(selection: key) {
            ForEach(engines, id: \.key) { engine in
                if sized {
                    let download = SetupState.download(of: engine, in: state.inspection)
                    Text("\(engine.label)   \(Text(download.brief).foregroundStyle(.secondary))")
                        .accessibilityLabel("\(engine.label), \(download.note ?? download.brief)")
                        .tag(engine.key)
                } else {
                    Text(engine.label).tag(engine.key)
                }
            }
            // config.yaml may name an engine setup does not offer; the
            // picker still needs a row for it.
            if chosen == nil {
                Text("\(key.wrappedValue) (from config.yaml)").tag(key.wrappedValue)
            }
        } label: {
            Text(title)
            if let chosen, !chosen.note.isEmpty {
                Text(chosen.note)
                    .foregroundStyle(chosen.availability == .unsupported ? Color.red : Color.secondary)
            }
        }
    }
}

/// The models the chosen engines need, each one a switch.
private struct DownloadsSection: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Section {
            ForEach(SetupState.wanted(state.inspection, state.selections), id: \.name) { model in
                row(model)
            }
        } header: {
            Text("Downloads")
        } footer: {
            DownloadFooter(total: state.downloadTotal, notes: state.selfFetchingNotes)
        }
    }

    /// A downloaded or custom model keeps a disabled switch. Its status sits
    /// in the label so VoiceOver reads it with the switch.
    private func row(_ model: Aria_Setup_V1_Model) -> some View {
        let downloaded = model.onDisk == .complete
        let fixed = downloaded || model.external
        return Toggle(isOn: fixed ? .constant(downloaded) : toggle(model.name)) {
            Text(model.title)
            Text(Self.status(model)).monospacedDigit()
            if model.onDisk == .partial && !model.external {
                Text("Partly downloaded. Resumes where it stopped.")
            }
        }
        .disabled(fixed)
    }

    private static func status(_ model: Aria_Setup_V1_Model) -> String {
        if model.onDisk == .complete { return "Downloaded" }
        if model.external { return "Custom model" }
        return SetupState.downloadSize(of: [model])
    }

    private func toggle(_ name: String) -> Binding<Bool> {
        Binding(
            get: { state.downloads.contains(name) },
            set: { on in
                if on { state.downloads.insert(name) } else { state.downloads.remove(name) }
            }
        )
    }
}

struct InstallingPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                ForEach(state.installRows, id: \.step) { row in
                    LabeledContent {
                        mark(row.progress)
                    } label: {
                        Text(row.title).foregroundStyle(row.progress == .pending ? .secondary : .primary)
                    }
                    .accessibilityValue(spoken(row.progress))
                    if row.step == .models {
                        ForEach(state.downloadProgress.keys.sorted(), id: \.self) { model in
                            if let progress = state.downloadProgress[model] { bar(model, progress) }
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder private func mark(_ progress: SetupState.InstallRow.Progress) -> some View {
        switch progress {
        case .pending:
            StatusMark(status: .upcoming)
        case .running:
            ProgressView().controlSize(.small)
        case .done:
            StatusMark(status: .done)
        }
    }

    /// The mark is hidden from VoiceOver, so the row says its progress.
    private func spoken(_ progress: SetupState.InstallRow.Progress) -> String {
        switch progress {
        case .pending: "Not started"
        case .running: "In progress"
        case .done: "Done"
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
    }
}

struct GrantPage: View {
    var body: some View {
        Form {
            Section {
                Text("macOS asks to let Aria use Accessibility. Choose Open System Settings and switch on Aria.")
                Text("This page moves on by itself once the hotkey arms.")
                LabeledContent {
                    ProgressView().controlSize(.small)
                } label: {
                    Text("Waiting for the grant…").foregroundStyle(.secondary)
                }
            }
        }
    }
}

struct VoicePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                Text("Say a few sentences when asked. macOS may ask for the microphone.")
                TextField("Name", text: $state.voiceName, prompt: Text("As transcripts should show it"))
                if let prompt = state.voicePrompt {
                    Text(prompt).fontWeight(.medium)
                }
            } footer: {
                NoticeText(notice: state.notice)
            }
        }
    }
}

struct TryOutPage: View {
    private static let transcriptHeight: CGFloat = 72

    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                transcript
                    .frame(maxWidth: .infinity, minHeight: Self.transcriptHeight, alignment: .topLeading)
            } footer: {
                if let enrolled = state.enrolled {
                    Text(enrolled)
                }
                NoticeText(notice: state.notice)
            }
        }
    }

    @ViewBuilder private var transcript: some View {
        if let heard = state.heard {
            Text(heard)
                .font(.title3)
                .textSelection(.enabled)
        } else if state.busy {
            Label {
                Text("Listening…")
            } icon: {
                ProgressView().controlSize(.small)
            }
            .foregroundStyle(.secondary)
        } else {
            Text("What you say shows up here.").foregroundStyle(.tertiary)
        }
    }
}

struct DonePage: View {
    @ObservedObject var state: SetupState
    let dictationKey: HotkeyKey
    let askHotkey: Chord?

    var body: some View {
        Form {
            if state.selections.hotkey || state.selections.wake || askHotkey != nil {
                Section("How to use Aria") {
                    if state.selections.hotkey {
                        LabeledContent(
                            "Dictate",
                            value: state.hotkeyReady
                                ? "Tap \(dictationKey.title)" : "Works once Accessibility access is given"
                        )
                    }
                    if state.selections.wake {
                        LabeledContent("Dictate hands-free", value: "Say the wake word")
                    }
                    if let askHotkey {
                        LabeledContent("Ask your agent") { ChordLabel(chord: askHotkey) }
                    }
                }
            }
            if let summary = state.summary {
                Section {
                    ForEach(summary.models, id: \.model) { model in
                        LabeledContent(model.model, value: Self.outcome(model.outcome))
                    }
                    LabeledContent("Background service", value: "Registered")
                    if let heard = state.heard {
                        LabeledContent("Heard", value: heard)
                    }
                } footer: {
                    NoticeText(notice: state.notice)
                }
            }
        }
    }

    private static func outcome(_ outcome: Aria_Setup_V1_Outcome) -> String {
        switch outcome {
        case .downloaded: "Downloaded"
        case .present: "Already there"
        case .skipped: "Skipped"
        case .unspecified, .UNRECOGNIZED: "Unknown"
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

/// The total to download, over notes on engines that fetch their own models.
private struct DownloadFooter: View {
    let total: String
    let notes: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(total).fontWeight(.medium)
            ForEach(notes, id: \.self) { Text($0) }
        }
    }
}

private struct NoticeText: View {
    let notice: String?

    var body: some View {
        if let notice {
            Text(notice).foregroundStyle(.red)
        }
    }
}
