import Foundation

enum AssistantEdits {
    private static let maximumModelIDBytes = 200
    private static let invalidModelCharacters = CharacterSet.whitespacesAndNewlines.union(.controlCharacters)
    enum ModelSelection: Hashable {
        case defaultModel
        case model(String)
        case custom
    }

    static func modelID(in preferences: Aria_Daemon_V1_UserPreferences) -> String? {
        guard preferences.hasAssistantID, preferences.assistantID != .unspecified else { return nil }
        if case .UNRECOGNIZED = preferences.assistantID { return nil }
        return preferences.assistantModels.first { $0.assistantID == preferences.assistantID }?.modelID ?? ""
    }

    static func setModelID(
        _ modelID: String, for assistantID: Aria_Daemon_V1_AssistantID, in patch: inout Aria_Daemon_V1_PreferencePatch
    ) {
        var entry = Aria_Daemon_V1_AssistantModelPreference()
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
