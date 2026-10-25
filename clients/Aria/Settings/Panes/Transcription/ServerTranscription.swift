import Foundation

enum TranscriptionLocation: Equatable {
    case thisMac
    case server
}

/// Each switch runs setup apply, which writes the config and restarts the service.
@MainActor
final class ServerTranscription: ObservableObject {
    struct Effects {
        var inspect: () async throws -> Aria_Setup_V1_Inspection
        var pair: (_ invitation: String) async throws -> Void
        var forget: () async throws -> Void
        var apply: (Aria_Setup_V1_ApplyRequest) async throws -> Void
        var stopDaemon: () async -> Void
        var startDaemon: () async -> Void
        /// The config forgets the local engine while the server is in use.
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

    @Published private(set) var busy = false
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
        !busy && (setup.server != nil || setup.transcription == .server)
    }

    func use(_ location: TranscriptionLocation) async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        notice = nil
        do {
            let inspection = try await effects.inspect()
            let engine: String
            switch location {
            case .server:
                engine = SetupStatus.serverEngine
                if inspection.defaults.stt != SetupStatus.serverEngine {
                    effects.rememberLocalEngine(inspection.defaults.stt)
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
            notice = .failed(error.localizedDescription)
        }
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

    /// Only from This Mac: forgetting the server in use would fail every dictation.
    var canForget: Bool {
        !busy && setup.server != nil && setup.transcription == .thisMac
    }

    func forget() async {
        guard canForget else { return }
        busy = true
        defer { busy = false }
        notice = nil
        do {
            try await effects.forget()
        } catch {
            notice = .failed(error.localizedDescription)
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
    /// so the config takes the new address.
    func pair(invitation: String) async -> Bool {
        guard !busy else { return false }
        busy = true
        defer { busy = false }
        pairingFailure = nil
        notice = nil
        do {
            try await effects.pair(invitation)
        } catch {
            pairingFailure = error.localizedDescription
            // The invitation works once; a retry needs a fresh one.
            pairingStep = .instructions
            return false
        }
        do {
            let inspection = try await effects.inspect()
            if inspection.defaults.stt == SetupStatus.serverEngine {
                try await apply(SetupStatus.serverEngine, from: inspection)
            } else {
                setup.show(inspection)
            }
        } catch {
            notice = .failed(error.localizedDescription)
        }
        return true
    }

    static func localEngine(in inspection: Aria_Setup_V1_Inspection, preferring previous: String?) -> String? {
        let ready = inspection.stt.filter { hasModelOnDisk($0, in: inspection) }
        return ready.first { $0.key == previous }?.key ?? ready.first?.key
    }

    private static func hasModelOnDisk(_ engine: Aria_Setup_V1_Engine, in inspection: Aria_Setup_V1_Inspection) -> Bool
    {
        guard engine.availability == .ready else { return false }
        switch engine.model {
        case .builtIn: return true
        case .downloads:
            let models = inspection.models.filter { $0.engine == engine.key }
            return !models.isEmpty && models.allSatisfy { $0.onDisk == .complete || $0.external }
        case .fetchedOnFirstUse, .remote, .unspecified, .UNRECOGNIZED: return false
        }
    }

    private func apply(_ engine: String, from inspection: Aria_Setup_V1_Inspection) async throws {
        var request = Aria_Setup_V1_ApplyRequest()
        request.selections = inspection.defaults
        request.selections.stt = engine
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
            notice = .failed("Saved, but Aria couldn’t read it back: \(error.localizedDescription)")
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
