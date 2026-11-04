import GRPCCore
import GRPCProtobuf

enum SpeechClient {
    /// Returns once the text has been read aloud.
    static func speak(_ text: String) async throws {
        var request = Aria_Daemon_V1_SpeakRequest()
        request.text = text
        let sent = request
        try await DaemonTransport.call(timeout: .seconds(60)) { client, options in
            _ = try await Aria_Daemon_V1_SpeechService.Client(wrapping: client)
                .speak(request: ClientRequest(message: sent), options: options) { try $0.message }
        }
    }
}
