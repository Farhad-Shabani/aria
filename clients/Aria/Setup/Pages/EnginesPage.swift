import SwiftUI

struct EnginesPage: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                picker("Speech to text", state.inspection.transcriptionChoices, $state.selections.transcriptionEngineID)
                picker("Text to speech", state.inspection.synthesisChoices, $state.selections.synthesisEngineID)
            }
            Section {
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

    private func picker(_ title: String, _ engines: [Aria_Setup_V1_Choice], _ key: Binding<String>)
        -> some View
    {
        let chosen = engines.first { $0.id == key.wrappedValue }
        return Picker(selection: key) {
            ForEach(engines, id: \.id) { engine in
                let download = SetupState.download(of: engine, in: state.inspection)
                Text("\(engine.label)   \(Text(download.brief).foregroundStyle(.secondary))")
                    .accessibilityLabel("\(engine.label), \(download.note ?? download.brief)")
                    .tag(engine.id)
            }
            // config.yaml may name an engine setup does not offer; the
            // picker still needs a row for it.
            if chosen == nil {
                Text("\(key.wrappedValue) (from config.yaml)").tag(key.wrappedValue)
            }
        } label: {
            Text(title)
            if let chosen, !chosen.availabilityDetail.isEmpty {
                Text(chosen.availabilityDetail)
                    .foregroundStyle(chosen.availability == .unsupported ? Color.failure : Color.secondary)
            }
        }
    }
}

/// The models the chosen engines need, each one a switch.
private struct DownloadsSection: View {
    @ObservedObject var state: SetupState

    var body: some View {
        Section {
            ForEach(SetupState.wanted(state.inspection, state.selections), id: \.id) { model in
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
        return Toggle(isOn: fixed ? .constant(downloaded) : toggle(model.id)) {
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

    private func toggle(_ id: String) -> Binding<Bool> {
        Binding(
            get: { state.downloads.contains(id) },
            set: { on in
                if on { state.downloads.insert(id) } else { state.downloads.remove(id) }
            }
        )
    }
}
