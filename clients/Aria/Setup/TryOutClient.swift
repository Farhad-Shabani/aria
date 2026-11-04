import Foundation

enum TryOutFailure: LocalizedError {
    case stopped

    var errorDescription: String? {
        switch self {
        case .stopped: "The recording stopped before Aria heard anything. Try again."
        }
    }
}

/// The test press: one recording handed back as text.
enum TryOutClient {
    static func transcribe() async throws -> String {
        switch try await DictationClient.start(stop: .onSilence, projectDirectory: nil, timeout: .seconds(120)) {
        case .heard(let transcript): return transcript.text
        case .stopAccepted: throw TryOutFailure.stopped
        }
    }
}
