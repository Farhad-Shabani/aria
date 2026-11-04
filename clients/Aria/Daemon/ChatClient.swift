import Foundation
import GRPCCore
import GRPCProtobuf

/// An assistant the daemon can run.
struct Assistant: Equatable, Sendable {
    enum Status: Equatable, Sendable {
        case ready, notInstalled, signedOut, unchecked
    }

    /// The wire id, because settings stores the chosen assistant by it.
    let id: Aria_Daemon_V1_AssistantId
    let name: String
    let status: Status
    /// How to make an unready assistant ready, as the daemon writes it: a
    /// clause with commands in backticks.
    let fix: String

    init(id: Aria_Daemon_V1_AssistantId, name: String, status: Status, fix: String = "") {
        self.id = id
        self.name = name
        self.status = status
        self.fix = fix
    }

    init(_ wire: Aria_Daemon_V1_Assistant) throws {
        switch wire.id {
        case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
        default: break
        }
        let status: Status
        switch wire.status {
        case .ready: status = .ready
        case .notInstalled: status = .notInstalled
        case .signedOut: status = .signedOut
        case .unchecked: status = .unchecked
        case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
        }
        guard !wire.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            wire.name.utf8.count <= 200,
            !wire.name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else { throw DaemonFailure.invalidResponse }
        self.init(id: wire.id, name: wire.name, status: status, fix: wire.fix)
    }
}

struct AssistantModel: Equatable, Sendable {
    let id: String
    let name: String
}

/// Where an ask goes: the project in front, and the assistant chosen in settings.
struct AskTarget: Sendable {
    let workdir: String
    let assistantID: Aria_Daemon_V1_AssistantId
}

enum ChatClient {
    static func assistants() async throws -> [Assistant] {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .listAssistants(request: ClientRequest(message: .init()), options: options) {
                    try assistants(from: $0.message.assistants)
                }
        }
    }

    /// A list the settings picker can show: at least one, each known once.
    static func assistants(from wire: [Aria_Daemon_V1_Assistant]) throws -> [Assistant] {
        let assistants = try wire.map { try Assistant($0) }
        guard !assistants.isEmpty, Set(assistants.map(\.id)).count == assistants.count else {
            throw DaemonFailure.invalidResponse
        }
        return assistants
    }

    static func models(for assistantID: Aria_Daemon_V1_AssistantId) async throws -> [AssistantModel] {
        var request = Aria_Daemon_V1_ListAssistantModelsRequest()
        request.assistantID = assistantID
        let sent = request
        return try await DaemonTransport.call(timeout: .seconds(40)) { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .listModels(request: ClientRequest(message: sent), options: options) {
                    try $0.message.models.map { AssistantModel(id: $0.id, name: $0.name) }
                }
        }
    }

    static func resolve() async throws -> AskTarget {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .resolve(request: ClientRequest(message: .init()), options: options) {
                    let resolved = try $0.message
                    return AskTarget(workdir: resolved.workdir, assistantID: resolved.assistantID)
                }
        }
    }

    /// Starts an ask from a press and returns its ticket. The reply is typed
    /// where the user is looking, or copied when the app in front refuses
    /// insertion, and the Aria key cancels the run.
    static func ask(text: String, target: AskTarget, ownVoice: Bool) async throws -> UInt64 {
        var request = Aria_Daemon_V1_AskRequest()
        request.text = text
        request.workdir = target.workdir
        request.assistantID = target.assistantID
        request.outputMode = .insertOrCopy
        // The daemon's own verdict, handed back: it may remember what its
        // owner said.
        request.ownVoice = ownVoice
        request.tapCancels = true
        let sent = request
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .ask(request: ClientRequest(message: sent), options: options) { try $0.message.ticket }
        }
    }

    /// Runs a stored prompt from a press and returns its ticket. The result
    /// is typed only into `application`, the app that was in front.
    static func runPrompt(text: String, promptID: String, application: String?) async throws -> UInt64 {
        var request = Aria_Daemon_V1_RunPromptRequest()
        request.text = text
        request.promptID = promptID
        request.applicationID = application ?? ""
        request.tapCancels = true
        let sent = request
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .runPrompt(request: ClientRequest(message: sent), options: options) { try $0.message.ticket }
        }
    }
}
