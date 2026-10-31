import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupStatus")

/// The last setup inspection, as Settings shows it. Only steps dictation
/// needs raise the badge.
@MainActor
final class SetupStatus: ObservableObject {
    @Published private(set) var required: [Aria_Setup_V1_Missing] = []
    /// Optimistic until inspected: a failed inspection should not lock out
    /// voice setup, and enrolling reports a missing model itself.
    @Published private(set) var hasSpeakerModelOnDisk = true
    @Published private(set) var server: Aria_Setup_V1_PairedServer?
    @Published private(set) var transcription = TranscriptionLocation.thisMac

    var reason: String? { required.first.map(Self.explanation) }

    func show(_ inspection: Aria_Setup_V1_Inspection) {
        required = Self.required(in: inspection)
        hasSpeakerModelOnDisk = Self.hasSpeakerModelOnDisk(in: inspection)
        server = inspection.hasServer ? inspection.server : nil
        transcription = inspection.defaults.transcriptionEngineID == Self.serverEngine ? .server : .thisMac
    }

    func refresh() async {
        do {
            show(try await SetupClient.inspect())
        } catch {
            log.error("Setup inspection failed: \(String(describing: error), privacy: .private)")
        }
    }

    static func required(in inspection: Aria_Setup_V1_Inspection) -> [Aria_Setup_V1_Missing] {
        let recognition = Set(
            inspection.models.filter { $0.engineID == inspection.defaults.transcriptionEngineID }.map(\.id))
        return inspection.missing.filter { missing in
            switch missing.kind {
            case .grant: true
            case .model: recognition.contains(missing.modelID)
            case .config, .wakeword, .service, .unspecified, .UNRECOGNIZED: false
            }
        }
    }

    static let serverEngine = "server"

    /// Setup's name for the embedding model an enrollment needs.
    static let speakerModel = "speaker"

    static func hasSpeakerModelOnDisk(in inspection: Aria_Setup_V1_Inspection) -> Bool {
        inspection.models.contains { $0.id == speakerModel && ($0.onDisk == .complete || $0.external) }
    }

    private static func explanation(_ missing: Aria_Setup_V1_Missing) -> String {
        missing.kind == .grant ? "Aria key needs permission." : "Speech model isn’t downloaded."
    }
}
