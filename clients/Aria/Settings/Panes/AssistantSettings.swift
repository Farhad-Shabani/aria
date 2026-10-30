import SwiftUI

struct AssistantSettings: View {
    @ObservedObject var preferences: DaemonPreferences
    @ObservedObject var catalog: AssistantCatalog

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            ItemPage(title: "Assistant", subtitle: "Ask and Prompts") {
                SettingsIcon(symbol: "sparkles", side: SettingsMetrics.headerIconSide)
            } content: {
                Section {
                    Picker("Assistant", selection: editor.binding(\.assistantID, \.assistantID)) {
                        if !catalog.choices.contains(where: { $0.id == editor.snapshot.assistantID }) {
                            Text("Choose an assistant").tag(editor.snapshot.assistantID)
                        }
                        ForEach(catalog.choices, id: \.id) { assistant in
                            Text(assistant.pickerLabel).tag(assistant.id)
                        }
                    }
                    .disabled(catalog.isLoading || catalog.choices.isEmpty)
                    if catalog.failure == nil,
                        let selected = catalog.choices.first(where: { $0.id == editor.snapshot.assistantID }),
                        let problem = selected.problem
                    {
                        NoticeText(notice: problem)
                        Text(selected.fix)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                        Button("Check Again") { Task { await catalog.load() } }
                            .disabled(catalog.isLoading)
                    }
                    if catalog.isLoading {
                        HStack {
                            ProgressView().controlSize(.small)
                            Text("Loading assistants…").foregroundStyle(.secondary)
                        }
                    }
                    if catalog.failure != nil {
                        NoticeText(notice: catalog.failure)
                        Button("Try Again") { Task { await catalog.load() } }
                            .disabled(catalog.isLoading)
                    }
                } footer: {
                    Text("Requires its command-line app.")
                }
                if let modelID = AssistantEdits.modelID(in: editor.snapshot) {
                    AssistantModelSection(
                        assistantID: editor.snapshot.assistantID,
                        modelID: editor.binding(modelID) { value, patch in
                            AssistantEdits.setModelID(value, for: editor.snapshot.assistantID, in: &patch)
                        }
                    )
                    .id(editor.snapshot.assistantID)
                }
                if preferences.lastError != nil {
                    Section {
                        NoticeText(notice: "Aria couldn't confirm these preferences.")
                        Button("Try Again") { Task { await preferences.sync() } }
                    }
                }
            }
        }
    }
}

extension Aria_Daemon_V1_Assistant {
    fileprivate var pickerLabel: String {
        switch status {
        case .notInstalled: return "\(name) — not installed"
        case .signedOut: return "\(name) — not signed in"
        case .unchecked: return "\(name) — not checked"
        case .ready, .unspecified, .UNRECOGNIZED: return name
        }
    }

    fileprivate var problem: String? {
        switch status {
        case .notInstalled: return "\(name) isn't installed."
        case .signedOut: return "\(name) isn't signed in."
        case .unchecked: return "Aria couldn't check \(name)."
        case .ready, .unspecified, .UNRECOGNIZED: return nil
        }
    }
}
