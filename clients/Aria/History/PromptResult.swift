import Foundation

/// A prompt's reply, saved under the history entry it ran on.
struct PromptResult: Identifiable, Equatable {
    let id: UInt64
    let created: Date
    /// The prompt's name when it ran.
    let promptName: String
    let text: String

    static func countLabel(_ count: Int) -> String {
        count == 1 ? "1 result" : "\(count) results"
    }
}
