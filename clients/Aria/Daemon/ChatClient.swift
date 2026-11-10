import Foundation
import GRPCCore
import GRPCProtobuf

/// An assistant the daemon can run.
struct Assistant: Equatable, Sendable {
    enum Status: Equatable, Sendable, CaseIterable {
        case ready, notInstalled, signedOut, notConfigured, unreachable, modelMissing, modelUnset, unchecked
    }

    /// The wire id, because settings stores the chosen assistant by it.
    let id: Aria_Daemon_V1_AssistantId
    let name: String
    let status: Status
    /// How to make an unready assistant ready, as the daemon writes it: a
    /// clause with commands in backticks.
    let fix: String
    /// It has no default model, so the user must choose one.
    let needsModel: Bool

    init(
        id: Aria_Daemon_V1_AssistantId, name: String, status: Status, fix: String = "", needsModel: Bool = false
    ) {
        self.id = id
        self.name = name
        self.status = status
        self.fix = fix
        self.needsModel = needsModel
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
        case .notConfigured: status = .notConfigured
        case .unreachable: status = .unreachable
        case .modelMissing: status = .modelMissing
        case .modelUnset: status = .modelUnset
        case .unchecked: status = .unchecked
        case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
        }
        guard !wire.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            wire.name.utf8.count <= 200,
            !wire.name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else { throw DaemonFailure.invalidResponse }
        self.init(id: wire.id, name: wire.name, status: status, fix: wire.fix, needsModel: wire.needsModel)
    }
}

struct AssistantModel: Equatable, Sendable {
    let id: String
    let name: String
}

/// Where a new chat goes: the project in front, and the assistant chosen in settings.
struct ChatTarget: Sendable {
    let workdir: String
    let assistantID: Aria_Daemon_V1_AssistantId
}

/// A chat the daemon started and named, and its first ask's ticket.
struct StartedChat: Equatable, Sendable {
    let ticket: UInt64
    let chat: ChatKey
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

    static func resolve() async throws -> ChatTarget {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .resolve(request: ClientRequest(message: .init()), options: options) {
                    let resolved = try $0.message
                    return ChatTarget(workdir: resolved.workdir, assistantID: resolved.assistantID)
                }
        }
    }

    /// Sends the first question of a chat the daemon starts and names, in `target`'s folder.
    /// The reply comes back only as the ticket's REPLIED event: nothing is typed.
    static func start(text: String, at target: ChatTarget, ownVoice: Bool) async throws -> StartedChat {
        let sent = startRequest(text: text, at: target, ownVoice: ownVoice)
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .ask(request: ClientRequest(message: sent), options: options) { try started($0.message) }
        }
    }

    /// With no output mode, the daemon types nothing.
    static func startRequest(text: String, at target: ChatTarget, ownVoice: Bool) -> Aria_Daemon_V1_AskRequest {
        var request = Aria_Daemon_V1_AskRequest()
        request.text = text
        request.assistantID = target.assistantID
        request.start = .with { $0.workdir = target.workdir }
        request.ownVoice = ownVoice
        request.tapCancels = true
        return request
    }

    /// A daemon from before it started chats answers with no key, which leaves nothing to
    /// follow up in.
    static func started(_ response: Aria_Daemon_V1_AskResponse) throws -> StartedChat {
        guard response.hasChat else { throw DaemonFailure.invalidResponse }
        return StartedChat(ticket: response.ticket, chat: try ChatKey(response.chat))
    }

    /// Sends a question in `chat` and returns its ticket. The reply comes back only as the
    /// ticket's REPLIED event: nothing is typed.
    static func ask(
        text: String, in chat: ChatKey, folder: String?, ownVoice: Bool, tapCancels: Bool
    ) async throws -> UInt64 {
        let sent = askRequest(
            text: text, in: chat, folder: folder, ownVoice: ownVoice, tapCancels: tapCancels)
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .ask(request: ClientRequest(message: sent), options: options) { try $0.message.ticket }
        }
    }

    /// With no output mode, the daemon types nothing. It runs the ask in the folder it recorded
    /// for the chat.
    static func askRequest(
        text: String, in chat: ChatKey, folder: String?, ownVoice: Bool, tapCancels: Bool
    ) -> Aria_Daemon_V1_AskRequest {
        var request = Aria_Daemon_V1_AskRequest()
        request.text = text
        request.assistantID = chat.assistantID
        request.named = .with {
            $0.name = chat.name
            // Only seeds a chat whose folder record the daemon lost, such as to its record
            // cap, which would otherwise refuse the ask. A recorded folder wins.
            $0.workdir = folder ?? ""
        }
        // The daemon's own verdict, handed back: it may remember what its owner said.
        request.ownVoice = ownVoice
        request.tapCancels = tapCancels
        return request
    }

    /// Runs a stored prompt from a press and returns its ticket. The result
    /// is typed only into `application`, the app that was in front.
    static func runPrompt(text: String, promptID: String, application: String?) async throws -> UInt64 {
        var request = Aria_Daemon_V1_RunPromptRequest()
        request.text = text
        request.promptID = promptID
        request.applicationID = application ?? ""
        request.tapCancels = true
        request.outputMode = .insertOrCopy
        let sent = request
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .runPrompt(request: ClientRequest(message: sent), options: options) { try $0.message.ticket }
        }
    }

    /// Runs a prompt on a history entry and types nothing. The result comes back only as its
    /// ticket's REPLIED event. The daemon saves it under the entry when history is on.
    static func runPromptForReply(entry: UInt64, source: PromptSource) async throws -> UInt64 {
        let sent = replyRequest(entry: entry, source: source)
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .runPrompt(request: ClientRequest(message: sent), options: options) { try $0.message.ticket }
        }
    }

    /// With no output mode, the daemon types nothing.
    static func replyRequest(entry: UInt64, source: PromptSource) -> Aria_Daemon_V1_RunPromptRequest {
        var request = Aria_Daemon_V1_RunPromptRequest()
        request.historyEntryID = entry
        switch source {
        case .stored(let id): request.promptID = id
        case .oneOff(let instructions): request.instructions = instructions
        }
        return request
    }

    /// `chat`'s asks the daemon runs or has waiting, the running one first.
    static func pendingAsks(in chat: ChatKey) async throws -> [Aria_Daemon_V1_PendingAsk] {
        let sent = Aria_Daemon_V1_ListPendingAsksRequest.with { $0.chat = chat.wire }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .listPendingAsks(request: ClientRequest(message: sent), options: options) { try $0.message.asks }
        }
    }

    static func cancel(ticket: UInt64) async throws {
        let sent = Aria_Daemon_V1_CancelAskRequest.with { $0.ticket = ticket }
        _ = try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .cancel(request: ClientRequest(message: sent), options: options) { try $0.message.wasPending }
        }
    }

    /// Stops `chat`'s running ask and every one waiting behind it.
    static func cancel(chat: ChatKey) async throws {
        let sent = Aria_Daemon_V1_CancelChatRequest.with { $0.chat = chat.wire }
        _ = try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .cancelChat(request: ClientRequest(message: sent), options: options) {
                    try $0.message.cancelledCount
                }
        }
    }

    /// Deletes every saved ask of `chat` and forgets what continues it. One already gone succeeds;
    /// one with an ask running or waiting fails as busy.
    static func deleteChat(_ chat: ChatKey) async throws {
        let sent = Aria_Daemon_V1_DeleteChatRequest.with { $0.chat = chat.wire }
        _ = try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .deleteChat(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }

    /// Gives `chat` a new name, and answers it under the name the daemon kept. A name another chat
    /// of the assistant has fails as taken; an ask running or waiting under either name, as busy.
    static func renameChat(_ chat: ChatKey, to name: String) async throws -> ChatKey {
        let sent = Aria_Daemon_V1_RenameChatRequest.with {
            $0.chat = chat.wire
            $0.name = name
        }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .renameChat(request: ClientRequest(message: sent), options: options) {
                    let response = try $0.message
                    guard response.hasChat else { throw DaemonFailure.invalidResponse }
                    return try ChatKey(response.chat)
                }
        }
    }

    /// Writes a prompt's file when the request carries instructions, then its entry in the prompt list.
    static func savePrompt(
        _ request: Aria_Daemon_V1_SavePromptRequest
    ) async throws -> Aria_Daemon_V1_SavePromptResponse {
        let sent = request
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .savePrompt(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }

    /// Takes the prompt off the list. Its file stays where it is.
    static func removePrompt(id: String) async throws {
        let sent = Aria_Daemon_V1_RemovePromptRequest.with { $0.id = id }
        _ = try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .removePrompt(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }

    /// The prompt's file as a run reads it.
    static func getPrompt(id: String) async throws -> PromptDraft.Contents {
        try await getPrompt(.with { $0.promptID = id })
    }

    /// `file` as a run would read it.
    static func getPrompt(file: URL) async throws -> PromptDraft.Contents {
        try await getPrompt(.with { $0.file = file.standardizedFileURL.path })
    }

    private static func getPrompt(_ request: Aria_Daemon_V1_GetPromptRequest) async throws -> PromptDraft.Contents {
        do {
            let response = try await DaemonTransport.call { client, options in
                try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                    .getPrompt(request: ClientRequest(message: request), options: options) { try $0.message }
            }
            return try PromptDraft.Contents(response)
        } catch DaemonFailure.refused(.promptUnreadable) {
            return .unreadable
        }
    }

    /// The daemon types no more at once (`DeliverRequest.text`).
    static let maximumDeliveryBytes = 64 * 1024

    static func fitsDelivery(_ text: String) -> Bool {
        text.utf8.count <= maximumDeliveryBytes
    }

    /// Types `text` into `application`, the app that was in front, or copies
    /// it where that app refuses insertion.
    static func deliver(text: String, application: String?) async throws -> Aria_Daemon_V1_DeliveryOutcome {
        var request = Aria_Daemon_V1_DeliverRequest()
        request.text = text
        request.applicationID = application ?? ""
        request.outputMode = .insertOrCopy
        let sent = request
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_ChatService.Client(wrapping: client)
                .deliver(request: ClientRequest(message: sent), options: options) { try $0.message.outcome }
        }
    }
}

extension PromptDraft.Contents {
    init(_ response: Aria_Daemon_V1_GetPromptResponse) throws {
        switch response.contents {
        case .instructions(let text): self = .text(text)
        case .missing: self = .missing
        case nil: throw DaemonFailure.invalidResponse
        }
    }
}

extension ChatKey {
    init(_ wire: Aria_Daemon_V1_ChatKey) throws {
        switch wire.assistantID {
        case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
        default: break
        }
        guard !wire.name.isEmpty else { throw DaemonFailure.invalidResponse }
        self.init(assistantID: wire.assistantID, name: wire.name)
    }

    var wire: Aria_Daemon_V1_ChatKey {
        .with {
            $0.assistantID = assistantID
            $0.name = name
        }
    }
}
