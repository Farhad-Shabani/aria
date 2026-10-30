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
                            Text(assistant.name).tag(assistant.id)
                        }
                    }
                    .disabled(catalog.isLoading || catalog.choices.isEmpty)
                    if catalog.failure == nil,
                        let selected = catalog.choices.first(where: { $0.id == editor.snapshot.assistantID }),
                        let problem = selected.problem
                    {
                        AssistantStatusRow(problem: problem, fix: selected.fixSentence) {
                            if selected.status == .unchecked {
                                Button("Try Again") { Task { await catalog.load() } }
                                    .disabled(catalog.isLoading)
                            }
                        }
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
                // Both lists come from the daemon, so one failure covers both.
                if catalog.failure == nil, let modelID = AssistantEdits.modelID(in: editor.snapshot) {
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

/// One unready assistant, laid out as System Settings shows an account
/// that needs attention. The window rechecks it on coming to the front.
private struct AssistantStatusRow<Action: View>: View {
    let problem: String
    let fix: String
    @ViewBuilder let action: Action

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.attention)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(problem)
                Text((try? AttributedString(markdown: fix)) ?? AttributedString(fix))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Spacer()
            action
        }
    }
}

extension Aria_Daemon_V1_Assistant {
    /// The daemon writes its hint as a clause, with commands in backticks.
    fileprivate var fixSentence: String {
        guard let first = fix.first else { return fix }
        let sentence = first.uppercased() + fix.dropFirst()
        return sentence.hasSuffix(".") ? sentence : sentence + "."
    }

    fileprivate var problem: String? {
        switch status {
        case .notInstalled: return "\(name) isn't installed"
        case .signedOut: return "\(name) isn't signed in"
        case .unchecked: return "Aria couldn't check \(name)"
        case .ready, .unspecified, .UNRECOGNIZED: return nil
        }
    }
}
