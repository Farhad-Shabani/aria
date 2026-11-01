import Foundation

/// The daemon calls are injected so tests can drive every step without a
/// microphone.
@MainActor
final class VoiceProfiles: ObservableObject {
    struct Effects {
        var list: () async throws -> [Aria_Daemon_V1_EnrolledSpeaker]
        var enroll: (_ name: String, _ role: Aria_Daemon_V1_SpeakerRole, _ replace: Bool) async throws -> Void
        var remove: (_ name: String) async throws -> Void
        var setOwner: (_ name: String) async throws -> Void
    }

    struct Enrollment {
        enum Phase: Equatable {
            case naming
            case recording(taken: Int, listening: Bool)
        }

        let role: Aria_Daemon_V1_SpeakerRole
        let replacing: String?
        var phase = Phase.naming
        var failure: String?
    }

    static let samples = Int(SpeakerClient.samples)
    /// One per sample, together covering most English sounds. A finished
    /// sentence gives each recording a natural end: the daemon closes a
    /// sample only after a pause.
    static let sentences = [
        "A gentle breeze moved through the tall green grass.",
        "She keeps fresh bread and cheese in a wooden box.",
        "Thick fog rolled over the harbor before sunrise.",
        "Bright paper lanterns hung above the quiet street.",
        "We packed our jackets and walked along the shore.",
    ]

    /// Nil until the first list arrives, so the pane never flashes an empty state.
    @Published private(set) var speakers: [Aria_Daemon_V1_EnrolledSpeaker]?
    @Published private(set) var failure: String?
    @Published private(set) var enrollment: Enrollment?
    @Published var name = ""

    private let effects: Effects
    private var recording: Task<Void, Never>?
    /// Only the newest refresh may update the list. An older reply could
    /// bring back a voice the person just removed.
    private var refreshID: UUID?

    init(effects: Effects) {
        self.effects = effects
    }

    var hasOwner: Bool {
        speakers?.contains { $0.role == .owner } ?? false
    }

    /// Nil with no voice yet: the row that sets one up already says so.
    var ownVoiceSummary: String? {
        if hasOwner { return "Types only your voice. The status pill glows when Aria hears you." }
        if speakers?.isEmpty == false { return "Make one voice the owner to use this." }
        return nil
    }

    /// A refresh cancelled by leaving the pane is not a failure worth showing.
    func refresh() async {
        let refreshID = UUID()
        self.refreshID = refreshID
        do {
            let speakers = try await effects.list()
            guard self.refreshID == refreshID else { return }
            self.speakers = speakers
            failure = nil
        } catch {
            guard !Task.isCancelled, self.refreshID == refreshID else { return }
            failure = error.localizedDescription
        }
    }

    /// Asks for owner whenever none exists.
    func beginAdding() {
        name = ""
        enrollment = Enrollment(role: hasOwner ? .guest : .owner, replacing: nil)
    }

    func beginRerecording(_ speaker: Aria_Daemon_V1_EnrolledSpeaker) {
        name = speaker.name
        enrollment = Enrollment(role: speaker.role, replacing: speaker.name)
    }

    func start() {
        guard var enrollment, enrollment.phase == .naming else { return }
        let name = name.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        enrollment.phase = .recording(taken: 0, listening: false)
        enrollment.failure = nil
        self.enrollment = enrollment
        recording = Task {
            do {
                try await effects.enroll(name, enrollment.role, enrollment.replacing != nil)
                if !Task.isCancelled { self.enrollment = nil }
                // The voice is saved even when Cancel came too late, so the
                // list refreshes outside this possibly cancelled task.
                Task { await self.refresh() }
            } catch {
                // Cancel already closed the sheet; the error is the cancellation itself.
                guard !Task.isCancelled else { return }
                self.enrollment?.phase = .naming
                self.enrollment?.failure = error.localizedDescription
            }
        }
    }

    /// Dropping the call cancels the recording in the daemon.
    func cancel() {
        recording?.cancel()
        recording = nil
        enrollment = nil
    }

    func speakerEvent(_ event: Aria_Daemon_V1_SpeakerEvent) {
        guard case .recording = enrollment?.phase else { return }
        switch event.stage {
        case .recording: enrollment?.phase = .recording(taken: Int(event.index) - 1, listening: true)
        case .sampleTaken: enrollment?.phase = .recording(taken: Int(event.index), listening: false)
        case .unspecified, .enrolled, .cancelled, .failed, .UNRECOGNIZED: break
        }
    }

    func remove(_ name: String) async {
        do {
            try await effects.remove(name)
            await refresh()
        } catch {
            failure = error.localizedDescription
        }
    }

    /// The list is read back either way: the daemon demotes the old owner,
    /// and a failure partway can leave two owners.
    func makeOwner(_ name: String) async {
        var reason: String?
        do {
            try await effects.setOwner(name)
        } catch {
            reason = error.localizedDescription
        }
        await refresh()
        // A clean read clears the failure; this one must still show.
        if let reason { failure = reason }
    }
}

extension VoiceProfiles.Effects {
    @MainActor static var live: Self {
        Self(
            list: { try await SpeakerClient.list() },
            enroll: { name, role, replace in
                _ = try await SpeakerClient.enroll(name: name, role: role, replace: replace)
            },
            remove: { name in try await SpeakerClient.remove(name: name) },
            setOwner: { name in try await SpeakerClient.setOwner(name: name) }
        )
    }
}
