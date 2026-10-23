import SwiftUI

struct WelcomePage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        HStack(alignment: .top, spacing: 24) {
            AppIcon(side: 96)
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
                if state.step == .welcome { expressDownloads.padding(.top, 8) }
            }
        }
        .padding(.top, 8)
    }

    private var expressDownloads: some View {
        let plan = SetupState.expressPlan(state.inspection, state.selections)
        return VStack(alignment: .leading, spacing: 4) {
            if !plan.isEmpty {
                Text("Express Setup downloads:").font(.callout).foregroundStyle(.secondary)
                ForEach(plan, id: \.name) { model in
                    HStack {
                        Text(model.title)
                        Spacer()
                        Text(SetupState.downloadSize(of: [model])).foregroundStyle(.secondary).monospacedDigit()
                    }
                    .accessibilityElement(children: .combine)
                }
            }
            Text(SetupState.total(plan)).fontWeight(.medium)
            ForEach(state.selfFetchingNotes, id: \.self) { Text($0).font(.callout).foregroundStyle(.secondary) }
        }
    }
}

struct EnginesPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            LabeledContent("Speech to text:") {
                picker("Speech to text", state.inspection.stt, $state.selections.stt, sized: true)
            }
            LabeledContent("Text to speech:") {
                picker("Text to speech", state.inspection.tts, $state.selections.tts, sized: true)
            }
            LabeledContent("Chat agent:") {
                picker("Chat agent", state.inspection.chat, $state.selections.chat, sized: false)
            }
            LabeledContent("Wake word:") {
                Toggle("Listen for the wake word", isOn: $state.selections.wake)
            }
            .setupGroup()
            if state.inspection.macos {
                LabeledContent("Hotkey:") {
                    VStack(alignment: .leading, spacing: 4) {
                        Toggle("Start dictation with a hotkey", isOn: $state.selections.hotkey)
                        Text("Needs the Accessibility grant; the next pages walk you through it.")
                            .setupFootnote()
                    }
                }
            }
        }
        .formStyle(.columns)
    }

    /// With `sized`, each row shows its download size for comparing. The
    /// chosen engine's note goes under the menu, where a long path fits.
    private func picker(_ title: String, _ engines: [Aria_Setup_V1_Engine], _ key: Binding<String>, sized: Bool)
        -> some View
    {
        let chosen = engines.first { $0.key == key.wrappedValue }
        return VStack(alignment: .leading, spacing: 4) {
            Picker(title, selection: key) {
                ForEach(engines, id: \.key) { engine in
                    if sized {
                        let download = SetupState.download(of: engine, in: state.inspection)
                        (Text(engine.label) + Text("   \(download.brief)").foregroundStyle(.secondary))
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
            }
            .labelsHidden()
            if sized, let chosen, let note = SetupState.download(of: chosen, in: state.inspection).note {
                Text(note).font(.callout).foregroundStyle(.secondary)
            }
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
        VStack(alignment: .leading, spacing: 12) {
            ForEach(SetupState.wanted(state.inspection, state.selections), id: \.name) { model in
                VStack(alignment: .leading, spacing: 2) {
                    row(model)
                    if model.onDisk == .partial && !model.external {
                        Text("Partly downloaded. Resumes where it stopped.").setupFootnote()
                    }
                }
            }
            Text(state.downloadTotal).fontWeight(.medium).padding(.top, 4)
            ForEach(state.selfFetchingNotes, id: \.self) { Text($0).setupFootnote() }
        }
    }

    /// A downloaded or custom model shows a disabled checkbox, so every
    /// title lines up. Its status sits in the label so VoiceOver reads it
    /// with the checkbox.
    private func row(_ model: Aria_Setup_V1_Model) -> some View {
        let downloaded = model.onDisk == .complete
        let fixed = downloaded || model.external
        return Toggle(isOn: fixed ? .constant(downloaded) : toggle(model.name)) {
            HStack {
                Text(model.title)
                Spacer()
                Text(Self.status(model)).foregroundStyle(.secondary).monospacedDigit()
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
            Text("macOS asks to let Aria use Accessibility. Choose Open System Settings and switch on Aria.")
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
                if let notice = state.notice { Text(notice).foregroundStyle(.red) }
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
