import Foundation
import os

/// The microphones the picker offers, read from the daemon while the picker
/// shows. Devices come and go, AirPods above all, so the list refreshes.
@MainActor
final class Microphones: ObservableObject {
    private static let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "Microphones")

    /// The daemon's word for the system default.
    static let automatic = "automatic"
    static let refreshInterval = Duration.seconds(2)
    /// Names of devices seen before, so a chosen one that isn't connected
    /// still shows by name. A presentation choice, so it stays in the app.
    static let namesKey = "aria.microphoneNames"

    struct Choice: Hashable, Identifiable {
        let id: String
        let title: String
    }

    /// Nil until the first list arrives.
    @Published private(set) var list: MicrophoneList?
    @Published private(set) var failure: String?

    private let fetch: @MainActor () async throws -> MicrophoneList
    private let defaults: UserDefaults

    init(
        fetch: @escaping @MainActor () async throws -> MicrophoneList = MicrophoneClient.list,
        defaults: UserDefaults = .standard
    ) {
        self.fetch = fetch
        self.defaults = defaults
    }

    var names: [String: String] {
        defaults.dictionary(forKey: Self.namesKey) as? [String: String] ?? [:]
    }

    /// Refreshes until the calling task is cancelled.
    func run() async {
        while !Task.isCancelled {
            await refresh()
            try? await Task.sleep(for: Self.refreshInterval)
        }
    }

    func refresh() async {
        do {
            let list = try await fetch()
            guard !Task.isCancelled else { return }
            self.list = list
            failure = nil
            remember(list.microphones)
        } catch {
            guard !Task.isCancelled else { return }
            // Refreshed every couple of seconds, so only a new failure is logged.
            if failure == nil {
                Self.log.error("Listing microphones failed: \(String(describing: error), privacy: .private)")
            }
            failure = Self.failureMessage(error)
        }
    }

    private func remember(_ microphones: [ListedMicrophone]) {
        var names = names
        let before = names
        for microphone in microphones { names[microphone.id] = microphone.label }
        if names != before { defaults.set(names, forKey: Self.namesKey) }
    }

    /// Automatic, then each connected device. A chosen device that isn't
    /// connected stays listed, so the picker still shows the choice.
    static func choices(listed: MicrophoneList?, chosen: String, names: [String: String]) -> [Choice] {
        var choices = [Choice(id: automatic, title: "Automatic")]
        let microphones = listed?.microphones ?? []
        choices += microphones.map { Choice(id: $0.id, title: $0.label) }
        if chosen != automatic, !microphones.contains(where: { $0.id == chosen }) {
            let name = names[chosen] ?? "Unknown Microphone"
            // Before the first list, whether it's connected is unknown.
            choices.append(Choice(id: chosen, title: listed == nil ? name : "\(name) (Not Connected)"))
        }
        return choices
    }

    /// What went wrong, then what to do. The daemon's own text stays in the log.
    static func failureMessage(_ error: any Error) -> String {
        let failure = error as? DaemonFailure ?? DaemonFailure(error)
        if failure.isOffline { return DaemonFailure.offlineMessage }
        if failure == .refused(.captureFailed) {
            return "Aria couldn’t list your microphones. Check them in System Settings > Sound."
        }
        return "Something went wrong. Try again."
    }

    /// Which device records, when the choice alone doesn't say.
    static func caption(listed: MicrophoneList?, chosen: String, names: [String: String]) -> String? {
        guard let listed else { return nil }
        guard let currentID = listed.currentID else { return "No microphone is connected." }
        let current = listed.microphones.first { $0.id == currentID }?.label ?? "the system default"
        if chosen == automatic { return "Records with \(current)." }
        if chosen == currentID { return nil }
        let name = names[chosen] ?? "The chosen microphone"
        return "\(name) isn’t connected, so Aria records with \(current)."
    }
}
