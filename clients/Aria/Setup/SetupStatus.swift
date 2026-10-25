import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "SetupStatus")

/// The setup steps dictation cannot work without. Optional ones never
/// raise the Settings badge.
@MainActor
final class SetupStatus: ObservableObject {
    @Published private(set) var required: [Aria_Setup_V1_Missing] = []
    /// Optimistic until inspected: a failed inspection should not lock out
    /// voice setup, and enrolling reports a missing model itself.
    @Published private(set) var speakerModelOnDisk = true

    var reason: String? { required.first.map(Self.explanation) }

    func show(_ inspection: Aria_Setup_V1_Inspection) {
        required = Self.required(in: inspection)
        speakerModelOnDisk = Self.speakerModelOnDisk(in: inspection)
    }

    func refresh() async {
        do {
            show(try await SetupClient.inspect())
        } catch {
            log.error("Setup inspection failed: \(String(describing: error), privacy: .private)")
        }
    }

    static func required(in inspection: Aria_Setup_V1_Inspection) -> [Aria_Setup_V1_Missing] {
        let recognition = Set(inspection.models.filter { $0.engine == inspection.defaults.stt }.map(\.name))
        return inspection.missing.filter { missing in
            switch missing.kind {
            case .grant: true
            case .model: recognition.contains(missing.model)
            case .config, .wakeword, .service, .unspecified, .UNRECOGNIZED: false
            }
        }
    }

    /// Setup's name for the embedding model an enrollment needs.
    static let speakerModel = "speaker"

    static func speakerModelOnDisk(in inspection: Aria_Setup_V1_Inspection) -> Bool {
        inspection.models.contains { $0.name == speakerModel && ($0.onDisk == .complete || $0.external) }
    }

    private static func explanation(_ missing: Aria_Setup_V1_Missing) -> String {
        missing.kind == .grant ? "Accessibility access is off." : "Speech model isn’t downloaded."
    }
}
