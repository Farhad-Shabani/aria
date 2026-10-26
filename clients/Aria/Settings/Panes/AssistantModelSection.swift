import SwiftUI

struct AssistantModelSection: View {
    let assistantID: Aria_Daemon_V1_AssistantId
    @Binding var modelID: String
    @StateObject private var models = AssistantModels()
    @State private var isChoosingCustomModel = false
    @State private var customModelID = ""

    private enum Selection: Hashable {
        case defaultModel
        case model(String)
        case custom
    }

    var body: some View {
        Section {
            LabeledContent("Model") {
                HStack {
                    Button("Refresh Models", systemImage: "arrow.clockwise") {
                        Task { await models.load(assistantID) }
                    }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                    .help("Refresh Models")
                    .disabled(models.isLoading)
                    Picker(
                        "Model",
                        selection: Binding(
                            get: { selection },
                            set: { select($0) }
                        )
                    ) {
                        Text("Default").tag(Selection.defaultModel)
                        if !modelID.isEmpty && !models.choices.contains(where: { $0.id == modelID }) {
                            Text(modelID).tag(Selection.model(modelID))
                        }
                        ForEach(models.choices, id: \.id) { choice in
                            Text(choice.name).tag(Selection.model(choice.id))
                        }
                        Text("Other…").tag(Selection.custom)
                    }
                    .labelsHidden()
                }
            }
            if isChoosingCustomModel {
                TextField("Model ID", text: $customModelID)
                    .onSubmit { useCustomModel() }
                Button("Use Model") { useCustomModel() }
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
        .task { await models.load(assistantID) }
    }

    private var selection: Selection {
        if isChoosingCustomModel { return .custom }
        if modelID.isEmpty { return .defaultModel }
        return .model(modelID)
    }

    private func select(_ selection: Selection) {
        switch selection {
        case .defaultModel: saveModel("")
        case .model(let modelID): saveModel(modelID)
        case .custom:
            customModelID = modelID
            isChoosingCustomModel = true
        }
    }

    private var canUseCustomModel: Bool {
        let modelID = customModelID.trimmingCharacters(in: .whitespacesAndNewlines)
        return !modelID.isEmpty && AssistantEdits.isValidModelID(modelID)
    }

    private func useCustomModel() {
        guard canUseCustomModel else { return }
        saveModel(customModelID.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private func saveModel(_ modelID: String) {
        self.modelID = modelID
        isChoosingCustomModel = false
    }
}
