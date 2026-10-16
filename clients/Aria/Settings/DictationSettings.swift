import SwiftUI

struct DictationSettings: View {
    @ObservedObject var preferences: DaemonPreferences
    @State private var showsAdvanced = false
    /// Held locally while the thumb moves; one push when it lands.
    @State private var silenceSeconds = DictationEdits.silenceSecondsRange.lowerBound

    var body: some View {
        Group {
            if let snapshot = preferences.snapshot {
                Form {
                    recognition(snapshot)
                    output(snapshot).settingsGroup()
                    audio(snapshot).settingsGroup()
                    advancedToggle.settingsGroup()
                    if showsAdvanced {
                        delivery(snapshot).settingsGroup()
                        voice(snapshot).settingsGroup()
                        vocabulary(snapshot).settingsGroup()
                        stopping(snapshot).settingsGroup()
                        history(snapshot).settingsGroup()
                    }
                }
                .formStyle(.columns)
            } else {
                Text("Waiting for the daemon…")
                    .foregroundStyle(.secondary)
            }
        }
        .padding(20)
        .onChange(of: preferences.snapshot?.silenceDurationMs, initial: true) { _, ms in
            if let ms { silenceSeconds = Double(ms) / 1000 }
        }
    }

    /// One form keeps the advanced rows on the same label column as the
    /// basic ones. The whole title is the click target; a DisclosureGroup
    /// on macOS only toggles from its triangle.
    private var advancedToggle: some View {
        LabeledContent("") {
            Button {
                showsAdvanced.toggle()
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .rotationEffect(.degrees(showsAdvanced ? 90 : 0))
                        .animation(.easeOut(duration: 0.15), value: showsAdvanced)
                    Text(showsAdvanced ? "Hide Advanced Options" : "Show Advanced Options")
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .accessibilityAddTraits(.isButton)
            .accessibilityValue(showsAdvanced ? "Expanded" : "Collapsed")
        }
    }

    private func recognition(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Language:") {
            Picker(
                "Language",
                selection: Binding(
                    get: { snapshot.recognitionLanguage },
                    set: { code in Task { await preferences.set { $0.recognitionLanguage = code } } }
                )
            ) {
                ForEach(DictationEdits.languageChoices(including: snapshot.recognitionLanguage), id: \.self) {
                    Text(DictationEdits.languageTitle($0)).tag($0)
                }
            }
            .labelsHidden()
            .fixedSize()
        }
    }

    private func output(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Output:") {
            VStack(alignment: .leading, spacing: 4) {
                Picker(
                    "Output",
                    selection: Binding(
                        get: { snapshot.outputMode },
                        set: { mode in Task { await preferences.set { DictationEdits.setOutput(mode, in: &$0) } } }
                    )
                ) {
                    ForEach(DictationEdits.outputModes, id: \.self) { mode in
                        Text(mode.title).tag(mode)
                    }
                }
                .labelsHidden()
                .fixedSize()
                if preferences.deliveryLocked {
                    Text("Copying always delivers after recording stops.")
                        .settingsFootnote()
                }
            }
        }
    }

    private func audio(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Audio:") {
            Toggle(
                "Mute output while recording",
                isOn: Binding(
                    get: { snapshot.muteOutput },
                    set: { on in Task { await preferences.set { $0.muteOutput = on } } }
                )
            )
        }
    }

    private func delivery(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Deliver:") {
            VStack(alignment: .leading, spacing: 4) {
                Picker(
                    "Deliver",
                    selection: Binding(
                        get: { snapshot.deliveryMode },
                        set: { mode in Task { await preferences.set { $0.deliveryMode = mode } } }
                    )
                ) {
                    Text("While speaking").tag(Aria_Daemon_V1_DeliveryMode.streaming)
                    Text("After recording stops").tag(Aria_Daemon_V1_DeliveryMode.complete)
                }
                .labelsHidden()
                .fixedSize()
                .disabled(preferences.deliveryLocked)
                Text("Same switch as Show Captions in the menu bar.")
                    .settingsFootnote()
            }
        }
    }

    private func voice(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Voice:") {
            VStack(alignment: .leading, spacing: 4) {
                Toggle(
                    "Only type my own voice",
                    isOn: Binding(
                        get: { snapshot.ownVoiceOnly },
                        set: { on in Task { await preferences.set { $0.ownVoiceOnly = on } } }
                    )
                )
                .help("Enroll your voice in Terminal: aria speaker enroll <name> --role owner")
                Text("Needs an enrolled owner voice.")
                    .settingsFootnote()
            }
        }
    }

    private func vocabulary(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        let directory = snapshot.hasVocabularyDirectory ? snapshot.vocabularyDirectory : nil
        return LabeledContent("Vocabulary:") {
            VStack(alignment: .leading, spacing: 4) {
                Toggle(
                    "Use project vocabulary",
                    isOn: Binding(
                        get: { snapshot.projectVocabulary },
                        set: { on in Task { await preferences.set { $0.projectVocabulary = on } } }
                    )
                )
                HStack {
                    Text(directory ?? "No folder chosen")
                        .foregroundStyle(directory == nil ? .secondary : .primary)
                        .truncationMode(.middle)
                        .lineLimit(1)
                    Button("Choose…", action: chooseVocabularyDirectory)
                    Button("Clear") {
                        Task { await preferences.set { $0.vocabularyDirectory = "" } }
                    }
                    .disabled(directory == nil)
                }
                .padding(.leading, 20)
                .disabled(!snapshot.projectVocabulary)
            }
        }
    }

    private func stopping(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("Stopping:") {
            VStack(alignment: .leading, spacing: 4) {
                Picker(
                    "Stop",
                    selection: Binding(
                        get: { snapshot.stopMode },
                        set: { mode in Task { await preferences.set { $0.stopMode = mode } } }
                    )
                ) {
                    Text("On silence").tag(Aria_Daemon_V1_StopMode.onSilence)
                    Text("Manually").tag(Aria_Daemon_V1_StopMode.manual)
                }
                .labelsHidden()
                .fixedSize()
                HStack {
                    Slider(
                        value: $silenceSeconds,
                        in: DictationEdits.silenceSecondsRange,
                        step: 0.1
                    ) { editing in
                        guard !editing else { return }
                        let ms = DictationEdits.silenceMilliseconds(seconds: silenceSeconds)
                        Task { await preferences.set { $0.silenceDurationMs = ms } }
                    }
                    .frame(width: 180)
                    Text(String(format: "%.1f s of silence", silenceSeconds))
                        .monospacedDigit()
                }
                .disabled(snapshot.stopMode != .onSilence)
                Text("Applies to wake-word and command-line dictation.")
                    .settingsFootnote()
            }
        }
    }

    private func history(_ snapshot: Aria_Daemon_V1_UserPreferences) -> some View {
        LabeledContent("History:") {
            VStack(alignment: .leading, spacing: 4) {
                Toggle(
                    "Keep recordings for retry",
                    isOn: Binding(
                        get: { snapshot.historyAudioRetention },
                        set: { on in Task { await preferences.set { $0.historyAudioRetention = on } } }
                    )
                )
                .disabled(!preferences.historyEnabled)
                Text("Lets you re-transcribe any dictation for 24 hours. Recordings may include other voices.")
                    .settingsFootnote()
            }
        }
    }

    private func chooseVocabularyDirectory() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Use Folder"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let path = url.standardizedFileURL.path
        Task { await preferences.set { $0.vocabularyDirectory = path } }
    }
}
