import SwiftUI

struct GeneralSettings: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var setup: SetupStatus
    @ObservedObject var settings: DaemonSettings
    @ObservedObject var catalog: AssistantCatalog
    let openAssistant: () -> Void
    @StateObject private var loginItem = LoginItem()
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.isEnabledByDefault
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.isEnabledByDefault
    @AppStorage(AppDelegate.captionsEnabledKey)
    private var captionsEnabled = AppDelegate.areCaptionsEnabledByDefault

    var body: some View {
        Form {
            Section("Startup") {
                Toggle(
                    "Open at login",
                    isOn: Binding(get: { loginItem.isEnabled }, set: { loginItem.set(enabled: $0) })
                )
            }
            Section {
                ItemLink(open: openAssistant) {
                    Text("Assistant")
                    Spacer()
                    Text(assistantName).foregroundStyle(.secondary)
                }
            }
            Section("On Screen") {
                Toggle("Show status pill", isOn: $statusPillEnabled)
                Toggle(isOn: $ghostTextEnabled) {
                    Text("Show inline preview")
                    Text("Words Aria hasn’t typed yet appear faintly at the cursor.")
                }
                Toggle(isOn: $captionsEnabled) {
                    Text("Show captions")
                    Text("What Aria hears appears in a panel while you speak.")
                }
            }
            if let snapshot = settings.snapshot {
                Section {
                    SpeakingRateRow(editor: SettingsEditor(snapshot: snapshot, settings: settings))
                } header: {
                    Text("Spoken Content")
                } footer: {
                    Text("Sets how fast Aria reads text aloud, as in Aria Setup and with `aria say`.")
                }
            }
            Section {
                LabeledContent {
                    if setup.required.isEmpty {
                        Button("Open…") { openWindow(id: WindowID.setup) }
                    } else {
                        Button("Finish Setup…") { openWindow(id: WindowID.setup) }
                            .buttonStyle(.borderedProminent)
                    }
                } label: {
                    // One branch per line count, so the row stays centered.
                    if let reason = setup.reason {
                        Text("Aria Setup")
                        Text(reason)
                    } else {
                        Text("Aria Setup")
                    }
                }
            }
        }
        .onAppear { loginItem.refresh() }
    }

    private var assistantName: String {
        guard let snapshot = settings.snapshot else {
            return settings.loadFailure == nil ? "Connecting…" : "Unavailable"
        }
        if let assistant = catalog.choices.first(where: { $0.id == snapshot.assistantID }) {
            return assistant.name
        }
        if catalog.isLoading { return "Loading…" }
        if catalog.failure != nil { return "Unavailable" }
        return "Choose…"
    }
}

private struct SpeakingRateRow: View {
    let editor: SettingsEditor
    /// Held locally while the thumb moves, then pushed once when it lands.
    @State private var rate = 1.0

    var body: some View {
        LabeledContent {
            HStack {
                // A step would make macOS draw tick marks, so the value is rounded when the thumb lands instead.
                Slider(value: $rate, in: DictationEdits.speakingRateRange) { editing in
                    guard !editing else { return }
                    rate = DictationEdits.roundedSpeakingRate(rate)
                    editor.set(\.ttsRate, to: rate)
                }
                .frame(maxWidth: 160)
                Text("\(rate, format: .number.precision(.fractionLength(2)))×")
                    .monospacedDigit()
            }
        } label: {
            Text("Speaking rate")
        }
        .onChange(of: editor.snapshot.ttsRate, initial: true) { _, stored in
            rate = stored
        }
    }
}
