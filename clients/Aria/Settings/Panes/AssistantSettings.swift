import SwiftUI

struct AssistantSettings: View {
    @ObservedObject var settings: DaemonSettings
    @ObservedObject var catalog: AssistantCatalog

    var body: some View {
        SettingsForm(settings: settings) { editor in
            let selected = catalog.choices.first { $0.id == editor.snapshot.assistantID }
            ItemPage(title: "Assistant", subtitle: "Chat and Prompts") {
                SettingsIcon(symbol: "sparkles", side: SettingsMetrics.headerIconSide)
            } content: {
                Section {
                    Picker("Assistant", selection: editor.binding(\.assistantID, \.assistantID)) {
                        if selected == nil {
                            Text("Choose an assistant").tag(editor.snapshot.assistantID)
                        }
                        ForEach(catalog.choices, id: \.id) { assistant in
                            Text(assistant.name).tag(assistant.id)
                        }
                    }
                    .disabled(catalog.choices.isEmpty)
                    if catalog.failure == nil, let selected, let problem = selected.problem {
                        AssistantStatusRow(problem: problem, fix: selected.fixSentence) {
                            if selected.status == .unchecked || selected.status == .unreachable {
                                if catalog.isLoading {
                                    ProgressView().controlSize(.small)
                                } else {
                                    Button("Try Again") { Task { await catalog.load() } }
                                }
                            }
                        }
                    }
                    if catalog.isLoading && catalog.choices.isEmpty {
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
                    if let requirement = selected?.requirement {
                        Text(requirement)
                    }
                }
                // Both lists come from the daemon, so one failure covers both.
                if catalog.failure == nil, let selected, let modelID = AssistantEdits.modelID(in: editor.snapshot) {
                    AssistantModelSection(
                        assistant: selected,
                        modelID: Binding(
                            get: { modelID },
                            set: { value in
                                Task {
                                    await settings.set {
                                        AssistantEdits.setModelID(value, for: selected.id, in: &$0)
                                    }
                                    // Readiness depends on the model, so check again.
                                    await catalog.load()
                                }
                            }
                        )
                    )
                    .id(selected.id)
                }
                if settings.lastError != nil {
                    Section {
                        NoticeText(notice: "Aria couldn’t confirm these settings.")
                        Button("Try Again") { Task { await settings.sync() } }
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

extension Assistant {
    /// The daemon writes its hint as a clause, with commands in backticks.
    fileprivate var fixSentence: String {
        guard let first = fix.first else { return fix }
        let sentence = first.uppercased() + fix.dropFirst()
        return sentence.hasSuffix(".") ? sentence : sentence + "."
    }

    fileprivate var requirement: String? {
        switch id {
        case .claudeCode, .codex: return "\(name) needs its command-line app."
        case .localModel: return "\(name) uses a model server on your Mac, such as Ollama."
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    var problem: String? {
        switch status {
        case .notInstalled: return "\(name) isn’t installed"
        case .signedOut: return "\(name) isn’t signed in"
        case .notConfigured: return "\(name) isn’t set up"
        case .unreachable: return "\(name) isn’t answering"
        case .modelMissing: return "\(name) isn’t available on the server"
        // The Model row is where the user fixes this.
        case .modelUnset: return nil
        case .unchecked: return "Aria couldn’t check \(name)"
        case .ready: return nil
        }
    }
}

extension Assistant.Status {
    /// An assistant that doesn't answer always has a `problem` that says why.
    var answers: Bool {
        switch self {
        case .ready, .modelMissing, .modelUnset: return true
        case .notInstalled, .signedOut, .notConfigured, .unreachable, .unchecked: return false
        }
    }
}
