import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupStatus")

enum RunsOn: Equatable {
    case local
    case server

    /// An unknown value reads as local, where the settings start.
    init(_ wire: Aria_Setup_V1_RunsOn) {
        self = wire == .server ? .server : .local
    }

    var wire: Aria_Setup_V1_RunsOn {
        switch self {
        case .local: .local
        case .server: .server
        }
    }
}

/// The last setup inspection, as Settings shows it.
@MainActor
final class SetupStatus: ObservableObject {
    @Published private(set) var items: [SetupItem] = []
    /// Optimistic until inspected: a failed inspection should not lock out
    /// voice setup, and enrolling reports a missing model itself.
    @Published private(set) var hasSpeakerModelOnDisk = true
    @Published private(set) var server: Aria_Setup_V1_PairedServer?
    @Published private(set) var transcriptionRunsOn = RunsOn.local
    @Published private(set) var speechModels = SpeechModels()
    @Published private(set) var downloadedModels = DownloadedModels()

    private let inspect: @MainActor () async throws -> Aria_Setup_V1_Inspection
    /// Only the newest inspection may update the status. An older one could
    /// hide a change the person just made.
    private var inspectionID: UUID?

    init(inspect: @escaping @MainActor () async throws -> Aria_Setup_V1_Inspection = SetupClient.inspect) {
        self.inspect = inspect
    }

    func show(_ inspection: Aria_Setup_V1_Inspection) {
        inspectionID = nil
        items = SetupItem.items(in: inspection)
        hasSpeakerModelOnDisk = Self.hasSpeakerModelOnDisk(in: inspection)
        server = inspection.hasServer ? inspection.server : nil
        transcriptionRunsOn = RunsOn(inspection.transcriptionRunsOn)
        speechModels = SpeechModels(inspection)
        downloadedModels = DownloadedModels(inspection)
    }

    func refresh() async {
        let inspectionID = UUID()
        self.inspectionID = inspectionID
        do {
            let inspection = try await inspect()
            guard self.inspectionID == inspectionID else { return }
            show(inspection)
        } catch {
            log.error("Setup inspection failed: \(String(describing: error), privacy: .private)")
        }
    }

    nonisolated static func isMissingTranscriptionModel(in inspection: Aria_Setup_V1_Inspection) -> Bool {
        SetupItem.items(in: inspection).contains {
            if case .speechModel = $0 { return true }
            return false
        }
    }

    /// Setup's name for the embedding model an enrollment needs.
    static let speakerModel = "speaker"

    static func hasSpeakerModelOnDisk(in inspection: Aria_Setup_V1_Inspection) -> Bool {
        inspection.models.contains { $0.id == speakerModel && ($0.files == .complete || $0.files == .external) }
    }
}
