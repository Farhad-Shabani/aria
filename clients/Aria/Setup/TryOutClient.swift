import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2

enum TryOutError: LocalizedError {
    case stopped
    case ended

    var errorDescription: String? {
        switch self {
        case .stopped: return "The recording was stopped before anything was heard."
        case .ended: return "The daemon ended the recording without a transcript."
        }
    }
}

/// The test press: one recording handed back as text, and one spoken reply.
enum TryOutClient {
    static func transcribe() async throws -> String {
        var request = Aria_Daemon_V1_StartDictationRequest()
        request.stopMode = .onSilence
        request.transcriptOnly = true
        request.deliveryMode = .complete
        var options = CallOptions.defaults
        options.timeout = .seconds(120)
        return try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            let dictation = Aria_Daemon_V1_DictationService.Client(wrapping: client)
            return try await dictation.start(request: ClientRequest(message: request), options: options) { response in
                for try await message in response.messages {
                    switch message.body {
                    case .result(let result): return result.text
                    case .stopAccepted: throw TryOutError.stopped
                    case .progress, nil: continue
                    }
                }
                throw TryOutError.ended
            }
        }
    }

    static func speak(_ text: String) async throws {
        var request = Aria_Daemon_V1_SpeakRequest()
        request.text = text
        var options = CallOptions.defaults
        options.timeout = .seconds(60)
        try await withGRPCClient(transport: DaemonTransport.unixSocket()) { client in
            let speech = Aria_Daemon_V1_SpeechService.Client(wrapping: client)
            _ = try await speech.speak(request: ClientRequest(message: request), options: options) { try $0.message }
        }
    }
}
