import SwiftUI

struct AssistantSettings: View {
    @ObservedObject var preferences: DaemonPreferences
    @StateObject private var models = AssistantModels()
    @ObservedObject var catalog: AssistantCatalog
    @State private var isChoosingCustomModel = false
    @State private var customModelID = ""

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            ItemPage(title: "Assistant", subtitle: "Ask and Prompts") {
                SettingsIcon(
                    symbol: "sparkles", tint: .indigo, side: SettingsMetrics.headerIconSide)
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
                    modelSection(modelID, editor)
                }
                if preferences.lastError != nil {
                    Section {
                        NoticeText(notice: "Aria couldn't confirm these preferences.")
                        Button("Try Again") { Task { await preferences.sync() } }
                    }
                }
            }
        }
        .task(id: preferences.snapshot?.assistantID) {
            guard let assistantID = preferences.snapshot?.assistantID else { return }
            await models.load(assistantID)
        }
        .onChange(of: preferences.snapshot?.assistantID) { _, _ in
            isChoosingCustomModel = false
            customModelID = ""
        }
    }

    private func modelSection(_ modelID: String, _ editor: PreferencesEditor) -> some View {
        Section {
            LabeledContent("Model") {
                HStack {
                    Button("Refresh Models", systemImage: "arrow.clockwise") {
                        Task { await models.load(editor.snapshot.assistantID) }
                    }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                    .help("Refresh Models")
                    .disabled(models.isLoading)
                    Picker(
                        "Model",
                        selection: Binding(
                            get: { selection(for: modelID) },
                            set: { select($0, current: modelID, editor: editor) }
                        )
                    ) {
                        Text("Default").tag(AssistantEdits.ModelSelection.defaultModel)
                        if !modelID.isEmpty && !models.choices.contains(where: { $0.id == modelID }) {
                            Text(modelID).tag(AssistantEdits.ModelSelection.model(modelID))
                        }
                        ForEach(models.choices, id: \.id) { choice in
                            Text(choice.name).tag(AssistantEdits.ModelSelection.model(choice.id))
                        }
                        Text("Other…").tag(AssistantEdits.ModelSelection.custom)
                    }
                    .labelsHidden()
                }
            }
            if isChoosingCustomModel {
                TextField("Model ID", text: $customModelID)
                    .onSubmit { useCustomModel(editor) }
                Button("Use Model") { useCustomModel(editor) }
                    .disabled(!canUseCustomModel)
            }
            if models.isLoading {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Loading models…").foregroundStyle(.secondary)
                }
            }
            NoticeText(notice: models.failure)
        }
    }

    private func selection(for modelID: String) -> AssistantEdits.ModelSelection {
        if isChoosingCustomModel { return .custom }
        if modelID.isEmpty { return .defaultModel }
        return .model(modelID)
    }

    private func select(_ selection: AssistantEdits.ModelSelection, current modelID: String, editor: PreferencesEditor)
    {
        switch selection {
        case .defaultModel: saveModel("", editor)
        case .model(let modelID): saveModel(modelID, editor)
        case .custom:
            customModelID = modelID
            isChoosingCustomModel = true
        }
    }

    private var canUseCustomModel: Bool {
        let modelID = customModelID.trimmingCharacters(in: .whitespacesAndNewlines)
        return !modelID.isEmpty && AssistantEdits.isValidModelID(modelID)
    }

    private func useCustomModel(_ editor: PreferencesEditor) {
        guard canUseCustomModel else { return }
        saveModel(customModelID.trimmingCharacters(in: .whitespacesAndNewlines), editor)
    }

    private func saveModel(_ modelID: String, _ editor: PreferencesEditor) {
        editor.set(modelID) { value, patch in
            AssistantEdits.setModelID(value, for: editor.snapshot.assistantID, in: &patch)
        }
        isChoosingCustomModel = false
    }
}
