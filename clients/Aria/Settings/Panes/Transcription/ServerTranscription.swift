import Foundation
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "ServerTranscription")

/// Each switch sets where transcription runs, and the service swaps engines
/// without a restart. The local engine stays configured on the server.
@MainActor
final class ServerTranscription: ObservableObject {
    struct Effects {
        var inspect: () async throws -> Aria_Setup_V1_Inspection
        var pair: (_ invitation: String) async throws -> Void
        var forget: () async throws -> Void
        var setRunsOn: (RunsOn, @escaping @Sendable (Aria_Setup_V1_SetupEvent) -> Void) async throws -> Void
    }

    /// Coming back from the server may first download the local engine's model.
    enum Progress: Equatable {
        case finishing
        case downloading(ModelTransfer?)
    }

    /// The pairing sheet shows the address an invitation names before it is
    /// spent, so a pasted invitation cannot redirect audio unseen.
    enum PairingStep: Equatable {
        case instructions
        case unreadable
        case confirming(invitation: String, address: String)
    }

    @Published private(set) var progress: Progress?
    @Published private(set) var failure: String?
    @Published private(set) var pairingFailure: String?
    @Published private(set) var pairingStep = PairingStep.instructions

    private let effects: Effects
    private let setup: SetupStatus
    private let runner: SetupRunner
    private var task: Task<Bool, Never>?

    init(effects: Effects, setup: SetupStatus, runner: SetupRunner) {
        self.effects = effects
        self.setup = setup
        self.runner = runner
    }

    /// Going to the server needs a pairing; coming back never does.
    var needsPairing: Bool {
        setup.server == nil && setup.transcriptionRunsOn != .server
    }

    var isBusy: Bool { progress != nil }

    var canChooseRunsOn: Bool {
        !isBusy && !needsPairing
    }

    /// A cancel can land after the write, so the change is read back even then.
    func use(_ runsOn: RunsOn) async {
        guard !isBusy else { return }
        progress = .finishing
        defer { progress = nil }
        failure = nil
        let run = Task { () -> Bool in
            do {
                try await runner.run {
                    try await effects.setRunsOn(runsOn) { event in
                        Task { @MainActor in self.absorb(event) }
                    }
                }
            } catch  where Task.isCancelled {
            } catch SetupClientError.cancelled {
            } catch {
                log.error("Switching transcription failed: \(String(describing: error), privacy: .private)")
                failure = SetupClientError.message(for: error, otherwise: Self.switchFailure)
                return false
            }
            return true
        }
        task = run
        let readBack = await run.value
        task = nil
        if readBack { await showChanges() }
    }

    func cancel() {
        task?.cancel()
    }

    private func absorb(_ event: Aria_Setup_V1_SetupEvent) {
        guard task != nil else { return }
        switch event.event {
        case .started(.models): progress = .downloading(nil)
        case .download(let report):
            if let transfer = ModelTransfer(report) { progress = .downloading(transfer) }
        case .started(.settings): progress = .finishing
        case .started, nil: break
        }
    }

    static let switchFailure = "Aria couldn’t change where transcription runs. Try again."
    static let forgetFailure = "Aria couldn’t forget the server. Try again."
    static let readBackFailure = "Saved, but Aria couldn’t show the change. Reopen Settings to see it."
    static let pairFailure = "Aria couldn’t pair with the server. Try again."
    static let invitationFailure = "That invitation didn’t work. Make a new invitation and try again."

    static func pairingMessage(for error: any Error) -> String {
        if error as? SetupClientError == .busy { return SetupClientError.busyMessage }
        return switch error as? PairingFailure {
        case .invitationInvalid: invitationFailure
        case .busy: SetupClientError.busyMessage
        case .unreachable: "Aria couldn’t reach the server. Make sure it’s on and reachable, then try again."
        case .timeout: "The server didn’t respond in time. Try again."
        case .permissionDenied:
            "The server can’t pair more devices. Run `aria remote revoke` on the server to remove one, then try again."
        case .failed, nil: pairFailure
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

    /// Only while transcription runs on your Mac: forgetting the server in use would fail every dictation.
    var canForget: Bool {
        !isBusy && setup.server != nil && setup.transcriptionRunsOn == .local
    }

    func forget() async {
        guard canForget else { return }
        progress = .finishing
        defer { progress = nil }
        failure = nil
        do {
            try await runner.run { try await effects.forget() }
        } catch {
            log.error("Forgetting the server failed: \(String(describing: error), privacy: .private)")
            failure = SetupClientError.message(for: error, otherwise: Self.forgetFailure)
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

    /// False when busy or pairing failed.
    func pair(invitation: String) async -> Bool {
        guard !isBusy else { return false }
        progress = .finishing
        defer { progress = nil }
        pairingFailure = nil
        failure = nil
        do {
            try await runner.run { try await effects.pair(invitation) }
        } catch {
            log.error("Pairing failed: \(String(describing: error), privacy: .private)")
            pairingFailure = Self.pairingMessage(for: error)
            pairingStep = .instructions
            return false
        }
        await showChanges()
        return true
    }

    /// The change has landed, so a failed read-back is not a failed change.
    private func showChanges() async {
        do {
            setup.show(try await effects.inspect())
        } catch {
            log.error("Reading back the change failed: \(String(describing: error), privacy: .private)")
            failure = Self.readBackFailure
        }
    }
}

extension ServerTranscription.Effects {
    @MainActor static var live: Self {
        Self(
            inspect: { try await SetupClient.inspect() },
            pair: { invitation in try await DaemonProcess.pairServer(invitation: invitation) },
            forget: { try await DaemonProcess.forgetServer() },
            setRunsOn: { runsOn, onEvent in
                var request = Aria_Setup_V1_SetTranscriptionRunsOnRequest()
                request.runsOn = runsOn.wire
                _ = try await SetupClient.setTranscriptionRunsOn(request, onEvent: onEvent)
            }
        )
    }
}
