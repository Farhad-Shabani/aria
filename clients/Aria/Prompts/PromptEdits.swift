/// Mirrors the daemon's checks on prompts.
enum PromptEdits {
    /// The daemon refuses more.
    static let maximumPrompts = 16
    static let kinds: [Aria_Daemon_V1_PromptKind] = [.rewrite, .compose]
}
