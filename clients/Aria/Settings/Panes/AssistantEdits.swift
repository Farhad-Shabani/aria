import Foundation

enum AssistantEdits {
    private static let maximumModelIDBytes = 200
    private static let invalidModelCharacters = CharacterSet.whitespacesAndNewlines.union(.controlCharacters)

    static func modelID(in settings: Aria_Daemon_V1_Settings) -> String? {
        guard settings.hasAssistantID, settings.assistantID != .unspecified else { return nil }
        if case .UNRECOGNIZED = settings.assistantID { return nil }
        return settings.assistantModels.first { $0.assistantID == settings.assistantID }?.modelID ?? ""
    }

    static func setModelID(
        _ modelID: String, for assistantID: Aria_Daemon_V1_AssistantId, in patch: inout Aria_Daemon_V1_SettingsPatch
    ) {
        var entry = Aria_Daemon_V1_AssistantModelSetting()
        entry.assistantID = assistantID
        entry.modelID = modelID
        patch.assistantModels.removeAll { $0.assistantID == assistantID }
        patch.assistantModels.append(entry)
    }

    static func isValidModelID(_ modelID: String) -> Bool {
        modelID.utf8.count <= maximumModelIDBytes && !modelID.hasPrefix("-")
            && !modelID.unicodeScalars.contains { invalidModelCharacters.contains($0) }
    }
}
