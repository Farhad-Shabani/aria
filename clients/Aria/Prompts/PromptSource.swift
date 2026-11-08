enum PromptSource: Equatable, Sendable {
    case stored(id: String)
    case oneOff(instructions: String)
}
