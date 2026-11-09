import SwiftUI

struct AssistantModelSection: View {
    let assistant: Assistant
    @Binding var modelID: String
    @StateObject private var models = AssistantModels()
    @State private var isChoosingCustomModel = false
    @State private var customModelID = ""

    private enum Selection: Hashable {
        case noModel
        case model(String)
        case custom
    }

    var body: some View {
        Section {
            LabeledContent("Model") {
                HStack {
                    Button("Refresh Models", systemImage: "arrow.clockwise") {
                        Task { await models.load(assistant.id) }
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
                        if let noModel = Self.noModelChoice(needsModel: assistant.needsModel, modelID: modelID) {
                            Text(noModel).tag(Selection.noModel)
                        }
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
            // The status row above says why an assistant that doesn't
            // answer can't list its models.
            if assistant.status.answers {
                NoticeText(notice: models.failure)
            }
        }
        .task(id: assistant.status.answers) {
            guard assistant.status.answers else { return }
            await models.load(assistant.id)
        }
    }

    static func noModelChoice(needsModel: Bool, modelID: String) -> String? {
        if !needsModel { return "Default" }
        return modelID.isEmpty ? "Choose a model" : nil
    }

    private var selection: Selection {
        if isChoosingCustomModel { return .custom }
        if modelID.isEmpty { return .noModel }
        return .model(modelID)
    }

    private func select(_ selection: Selection) {
        switch selection {
        case .noModel: saveModel("")
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
