import GRPCCore
import GRPCNIOTransportHTTP2
import GRPCProtobuf

enum AssistantClient {
    static func assistants() async throws -> [Aria_Daemon_V1_Assistant] {
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            let chat = Aria_Daemon_V1_ChatService.Client(wrapping: client)
            var options = CallOptions.defaults
            options.timeout = .seconds(5)
            return try await chat.listAssistants(
                request: ClientRequest(message: Aria_Daemon_V1_ListAssistantsRequest()), options: options
            ) {
                try $0.message.assistants
            }
        }
    }

    static func models(for assistantID: Aria_Daemon_V1_AssistantId) async throws -> [Aria_Daemon_V1_AssistantModel] {
        var request = Aria_Daemon_V1_ListAssistantModelsRequest()
        request.assistantID = assistantID
        let message = request
        return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            let chat = Aria_Daemon_V1_ChatService.Client(wrapping: client)
            var options = CallOptions.defaults
            options.timeout = .seconds(40)
            return try await chat.listModels(request: ClientRequest(message: message), options: options) {
                try $0.message.models
            }
        }
    }
}
