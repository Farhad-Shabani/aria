import Foundation

struct SpeechModels: Equatable {
    struct Model: Equatable, Identifiable {
        let id: String
        let label: String
        /// Nil when the model is on your Mac or needs no download.
        let size: String?
    }

    private(set) var models: [Model] = []
    /// Nil before the first inspection.
    private(set) var current: String?

    init() {}

    init(_ inspection: Aria_Setup_V1_Inspection) {
        current = inspection.defaults.sttEngineID
        models = inspection.sttChoices.compactMap { choice in
            guard choice.settable || choice.id == current else {
                return nil
            }
            let size: String? =
                switch SetupState.download(of: choice, in: inspection) {
                case .size(let size): size
                case .status: nil
                }
            return Model(id: choice.id, label: choice.label, size: size)
        }
    }

    var onMac: [Model] { models.filter { $0.size == nil } }
    var toDownload: [Model] { models.filter { $0.size != nil } }

    func model(_ id: String) -> Model? {
        models.first { $0.id == id }
    }
}
