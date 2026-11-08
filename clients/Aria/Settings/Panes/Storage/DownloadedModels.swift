import Foundation

struct DownloadedModels: Equatable {
    struct Model: Equatable, Identifiable {
        let id: String
        let name: String
        let bytes: UInt64
        let isPartial: Bool
        /// Empty when no setting loads it, so it can be deleted.
        let usedBy: [Aria_Setup_V1_ModelUse]
    }

    private(set) var models: [Model] = []

    init() {}

    init(_ inspection: Aria_Setup_V1_Inspection) {
        models = inspection.models
            .filter { $0.files == .complete || $0.files == .partial }
            .map { model in
                Model(
                    id: model.id,
                    name: model.label,
                    bytes: model.sizeBytes,
                    isPartial: model.files == .partial,
                    usedBy: model.usedBy
                )
            }
            // A partial download's size is what the whole would be, so it
            // can't rank against the rest.
            .sorted { ($0.isPartial ? 0 : $0.bytes) > ($1.isPartial ? 0 : $1.bytes) }
    }

    var total: UInt64 {
        models.filter { !$0.isPartial }.reduce(0) { $0 + $1.bytes }
    }
}

extension DownloadedModels.Model {
    var detail: String {
        let size = isPartial ? "Partly downloaded" : SetupState.size(bytes)
        guard !usedBy.isEmpty else { return size }
        return "\(size) · Used by \(usedBy.map(Self.feature).formatted(.list(type: .and)))"
    }

    private static func feature(_ use: Aria_Setup_V1_ModelUse) -> String {
        switch use {
        case .stt: "Dictation"
        case .systemTranscription: "Transcription"
        case .tts: "Spoken Content"
        case .speaker: "Voice"
        case .unspecified, .UNRECOGNIZED: "Aria"
        }
    }
}
