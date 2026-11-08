extension Aria_Daemon_V1_PromptKind {
    var title: String {
        switch self {
        case .rewrite: return "Rewrite them"
        case .compose: return "Write from them"
        case .unspecified, .UNRECOGNIZED: return "Unknown"
        }
    }
}
