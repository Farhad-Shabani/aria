import SwiftUI

struct GeneralSettings: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var settings: DaemonSettings
    @ObservedObject var catalog: AssistantCatalog
    @ObservedObject var setup: SetupStatus
    let openAssistant: () -> Void
    let openStorage: () -> Void
    @StateObject private var loginItem = LoginItem()
    @AppStorage(StatusController.enabledKey)
    private var statusPillEnabled = StatusController.isEnabledByDefault
    @AppStorage(GhostController.enabledKey)
    private var ghostTextEnabled = GhostController.isEnabledByDefault
    @AppStorage(CaptionState.enabledKey)
    private var captionsEnabled = CaptionState.isEnabledByDefault

    var body: some View {
        Form {
            Section("Startup") {
                Toggle(
                    SettingsRow.openAtLogin.title,
                    isOn: Binding(get: { loginItem.isEnabled }, set: { loginItem.set(enabled: $0) })
                )
                .settingsRow(.openAtLogin)
            }
            Section {
                ItemLink(open: openAssistant) {
                    Text(SettingsRow.assistant.title)
                    Spacer()
                    Text(assistantName).foregroundStyle(.secondary)
                }
                .settingsRow(.assistant)
            }
            Section("On Screen") {
                Toggle(SettingsRow.showStatusPill.title, isOn: $statusPillEnabled)
                    .settingsRow(.showStatusPill)
                Toggle(isOn: $ghostTextEnabled) {
                    Text(SettingsRow.showInlinePreview.title)
                    Text("Words Aria hasn’t typed yet appear faintly at the cursor.")
                }
                .settingsRow(.showInlinePreview)
                Toggle(isOn: $captionsEnabled) {
                    Text(SettingsRow.showCaptions.title)
                    Text("What Aria hears appears in a panel while you speak.")
                }
                .settingsRow(.showCaptions)
            }
            if let snapshot = settings.snapshot {
                Section {
                    SpeakingRateRow(editor: SettingsEditor(snapshot: snapshot, settings: settings))
                        .settingsRow(.speakingRate)
                } header: {
                    Text("Spoken Content")
                } footer: {
                    Text("Sets how fast Aria reads text aloud, as in Aria Setup and with `aria say`.")
                }
            }
            Section {
                ItemLink(open: openStorage) {
                    Text(SettingsRow.storage.title)
                    Spacer()
                    Text(SetupState.size(setup.downloadedModels.total)).foregroundStyle(.secondary)
                }
                .settingsRow(.storage)
            }
            Section {
                LabeledContent(SettingsRow.ariaSetup.title) {
                    Button("Open…") { openWindow(id: WindowID.setup) }
                }
                .settingsRow(.ariaSetup)
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
            Text(SettingsRow.speakingRate.title)
        }
        .onChange(of: editor.snapshot.ttsRate, initial: true) { _, stored in
            rate = stored
        }
    }
}
