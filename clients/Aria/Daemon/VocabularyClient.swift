import Foundation
import GRPCCore
import GRPCProtobuf

/// How adding or removing a word left your dictionary.
enum DictionaryEdit: Equatable, Sendable {
    case added(String)
    case alreadyListed(String)
    case removed(String)
    case notListed(String)

    var message: String {
        switch self {
        case .added(let word): "Added “\(word)” to your dictionary."
        case .alreadyListed(let word): "“\(word)” is already in your dictionary."
        case .removed(let word): "Removed “\(word)” from your dictionary."
        case .notListed(let word): "“\(word)” isn’t in your dictionary."
        }
    }
}

/// The daemon's VocabularyService, for your dictionary's words.
enum VocabularyClient {
    static func words() async throws -> [String] {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_VocabularyService.Client(wrapping: client)
                .listWords(request: ClientRequest(message: .init()), options: options) { try $0.message.words }
        }
    }

    static func add(_ word: String) async throws -> DictionaryEdit {
        let sent = Aria_Daemon_V1_AddWordRequest.with { $0.word = word }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_VocabularyService.Client(wrapping: client)
                .addWord(request: ClientRequest(message: sent), options: options) {
                    switch try $0.message.outcome {
                    case .added: .added(word)
                    case .alreadyListed: .alreadyListed(word)
                    case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
                    }
                }
        }
    }

    static func remove(_ word: String) async throws -> DictionaryEdit {
        let sent = Aria_Daemon_V1_RemoveWordRequest.with { $0.word = word }
        return try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_VocabularyService.Client(wrapping: client)
                .removeWord(request: ClientRequest(message: sent), options: options) {
                    switch try $0.message.outcome {
                    case .removed: .removed(word)
                    case .notListed: .notListed(word)
                    case .unspecified, .UNRECOGNIZED: throw DaemonFailure.invalidResponse
                    }
                }
        }
    }

    /// What the palette says when an edit fails.
    static func failureMessage(for error: Error) -> String {
        switch error as? DaemonFailure {
        case let failure? where failure.isOffline: DaemonFailure.offlineMessage
        case .refused(.invalidRequest): "Aria can’t use that word. Use a shorter word that doesn’t start with #."
        case .refused(.payloadTooLarge): "Your dictionary is full. Remove a word, then try again."
        default: "Aria couldn’t change your dictionary. Try again."
        }
    }
}
