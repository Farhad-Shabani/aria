import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "ServerTranscription")

enum TranscriptionLocation: Equatable {
    case thisMac
    case server
}

/// Each switch runs setup apply, which writes the settings file and restarts the service.
@MainActor
final class ServerTranscription: ObservableObject {
    struct Effects {
        var inspect: () async throws -> Aria_Setup_V1_Inspection
        var pair: (_ invitation: String) async throws -> Void
        var forget: () async throws -> Void
        var apply: (Aria_Setup_V1_ApplyRequest) async throws -> Void
        var stopDaemon: () async -> Void
        var startDaemon: () async -> Void
        /// The settings forget the local engine while the server is in use.
        var lastLocalEngine: () -> String?
        var rememberLocalEngine: (String) -> Void
    }

    /// The pairing sheet shows the address an invitation names before it is
    /// spent, so a pasted invitation cannot redirect audio unseen.
    enum PairingStep: Equatable {
        case instructions
        case unreadable
        case confirming(invitation: String, address: String)
    }

    enum Notice: Equatable {
        case failed(String)
        /// No local engine has its model on this Mac.
        case needsSetup
    }

    @Published private(set) var isBusy = false
    @Published private(set) var notice: Notice?
    @Published private(set) var pairingFailure: String?
    @Published private(set) var pairingStep = PairingStep.instructions

    private let effects: Effects
    private let setup: SetupStatus

    init(effects: Effects, setup: SetupStatus) {
        self.effects = effects
        self.setup = setup
    }

    /// Going to the server needs a pairing; coming back never does.
    var canChooseLocation: Bool {
        !isBusy && (setup.server != nil || setup.transcription == .server)
    }

    func use(_ location: TranscriptionLocation) async {
        guard !isBusy else { return }
        isBusy = true
        defer { isBusy = false }
        notice = nil
        do {
            let inspection = try await effects.inspect()
            let engine: String
            switch location {
            case .server:
                engine = SetupStatus.serverEngine
                if inspection.defaults.transcriptionEngineID != SetupStatus.serverEngine {
                    effects.rememberLocalEngine(inspection.defaults.transcriptionEngineID)
                }
            case .thisMac:
                guard let local = Self.localEngine(in: inspection, preferring: effects.lastLocalEngine()) else {
                    notice = .needsSetup
                    return
                }
                engine = local
            }
            try await apply(engine, from: inspection)
        } catch {
            log.error("Switching transcription failed: \(String(describing: error), privacy: .private)")
            notice = .failed(Self.switchFailure)
        }
    }

    static let switchFailure = "Aria couldn’t change where transcription runs. Try again."
    static let forgetFailure = "Aria couldn’t forget the server. Try again."
    static let readBackFailure = "Saved, but Aria couldn’t show the change. Reopen Settings to see it."
    static let pairFailure = "Aria couldn’t pair with the server. Try again."
    static let invitationFailure = "That invitation didn’t work. Make a new invitation and try again."

    /// The first line of each `aria remote pair` failure the user can act on,
    /// without its `error: ` prefix.
    private static let pairCauses: [(cliMessage: String, message: String)] = [
        ("remote invitation is invalid", invitationFailure),
        ("remote operation failed (unauthenticated)", invitationFailure),
        ("remote operation failed (invalid_request)", invitationFailure),
        (
            "another pairing operation is active",
            "Another pairing is in progress. Wait for it to finish, then try again."
        ),
        (
            "cannot establish the authenticated remote connection",
            "Aria couldn’t reach the server. Make sure it’s on and reachable, then try again."
        ),
        ("remote operation timed out", "The server didn’t respond in time. Try again."),
        (
            "remote operation failed (permission_denied)",
            "The server can’t pair more devices. Run `aria remote revoke` on the server to remove one, then try again."
        ),
    ]

    static func pairingMessage(for error: any Error) -> String {
        guard case .commandFailed(let detail)? = error as? DaemonClientError else { return pairFailure }
        let cause = detail.lowercased()
        return pairCauses.first { cause.hasPrefix($0.cliMessage) }?.message ?? pairFailure
    }

    func beginPairing() {
        pairingFailure = nil
        pairingStep = .instructions
    }

    func paste(_ clipboard: String) {
        pairingFailure = nil
        pairingStep =
            Self.invitationAddress(clipboard).map { .confirming(invitation: clipboard, address: $0) } ?? .unreadable
    }

    /// Only while transcription runs on your Mac: forgetting the server in use would fail every dictation.
    var canForget: Bool {
        !isBusy && setup.server != nil && setup.transcription == .thisMac
    }

    func forget() async {
        guard canForget else { return }
        isBusy = true
        defer { isBusy = false }
        notice = nil
        do {
            try await effects.forget()
        } catch {
            log.error("Forgetting the server failed: \(String(describing: error), privacy: .private)")
            notice = .failed(Self.forgetFailure)
            return
        }
        await showChanges()
    }

    /// Pairing still validates the invitation.
    static func invitationAddress(_ invitation: String) -> String? {
        struct Invitation: Decodable { let endpoint: String }
        guard let decoded = try? JSONDecoder().decode(Invitation.self, from: Data(invitation.utf8)) else { return nil }
        return address(of: decoded.endpoint)
    }

    /// `host:port` of an HTTPS origin, lowercased as pairing stores it.
    static func address(of endpoint: String) -> String? {
        guard let url = URLComponents(string: endpoint), url.scheme?.lowercased() == "https",
            let host = url.host?.lowercased(), !host.isEmpty
        else { return nil }
        return url.port.map { "\(host):\($0)" } ?? host
    }

    /// False when busy or pairing failed. A server in use is applied again,
    /// so the settings take the new address.
    func pair(invitation: String) async -> Bool {
        guard !isBusy else { return false }
        isBusy = true
        defer { isBusy = false }
        pairingFailure = nil
        notice = nil
        do {
            try await effects.pair(invitation)
        } catch {
            log.error("Pairing failed: \(String(describing: error), privacy: .private)")
            pairingFailure = Self.pairingMessage(for: error)
            pairingStep = .instructions
            return false
        }
        do {
            let inspection = try await effects.inspect()
            if inspection.defaults.transcriptionEngineID == SetupStatus.serverEngine {
                try await apply(SetupStatus.serverEngine, from: inspection)
            } else {
                setup.show(inspection)
            }
        } catch {
            log.error("Applying the paired server failed: \(String(describing: error), privacy: .private)")
            notice = .failed(Self.switchFailure)
        }
        return true
    }

    static func localEngine(in inspection: Aria_Setup_V1_Inspection, preferring previous: String?) -> String? {
        let ready = inspection.transcriptionChoices.filter { hasModelOnDisk($0, in: inspection) }
        return ready.first { $0.id == previous }?.id ?? ready.first?.id
    }

    private static func hasModelOnDisk(_ engine: Aria_Setup_V1_Choice, in inspection: Aria_Setup_V1_Inspection) -> Bool
    {
        guard engine.availability == .ready else { return false }
        switch engine.modelSource {
        case .builtIn: return true
        case .downloads:
            let models = inspection.models.filter { $0.engineID == engine.id }
            return !models.isEmpty && models.allSatisfy { $0.onDisk == .complete || $0.external }
        case .fetchedOnFirstUse, .remote, .unspecified, .UNRECOGNIZED: return false
        }
    }

    private func apply(_ engine: String, from inspection: Aria_Setup_V1_Inspection) async throws {
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = inspection.defaults
        request.selections.transcriptionEngineID = engine
        try await SetupClient.handingOffDaemon(inspection, stop: effects.stopDaemon, start: effects.startDaemon) {
            try await effects.apply(request)
        }
        await showChanges()
    }

    /// The change has landed, so a failed read-back is not a failed change.
    private func showChanges() async {
        do {
            setup.show(try await effects.inspect())
        } catch {
            log.error("Reading back the change failed: \(String(describing: error), privacy: .private)")
            notice = .failed(Self.readBackFailure)
        }
    }
}

extension ServerTranscription.Effects {
    private static let lastLocalEngineKey = "lastLocalEngine"

    @MainActor static var live: Self {
        Self(
            inspect: { try await SetupClient.inspect() },
            pair: { invitation in try await DaemonClient.pairServer(invitation: invitation) },
            forget: { try await DaemonClient.forgetServer() },
            apply: { request in _ = try await SetupClient.apply(request) { _ in } },
            stopDaemon: { await SetupClient.stopDaemonBeforeApply() },
            startDaemon: { await SetupClient.startDaemonAfterFailedApply() },
            lastLocalEngine: { UserDefaults.standard.string(forKey: lastLocalEngineKey) },
            rememberLocalEngine: { UserDefaults.standard.set($0, forKey: lastLocalEngineKey) }
        )
    }
}
