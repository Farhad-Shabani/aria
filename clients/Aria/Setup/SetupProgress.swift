struct SetupProgress {
    /// The rows of the setup rail, one noun each, as Installer names its
    /// steps. Each page's title says what the person does there. Done and
    /// failure sit outside them.
    enum Stage: Int, CaseIterable {
        case welcome
        case engines
        case install
        case hotkey
        case voice
        case tryOut
        case openAria

        var title: String {
            switch self {
            case .welcome: "Welcome"
            case .engines: "Engines"
            case .install: "Installation"
            case .hotkey: "Access"
            case .voice: "Voice"
            case .tryOut: "Dictation"
            case .openAria: "Shortcut"
            }
        }
    }

    enum Status {
        case done
        case current
        case upcoming
        case skipped
        case failed
    }

    private var step: SetupState.Step = .inspecting
    private var reached: Set<Stage> = []
    private var last: Stage?

    /// Inspecting begins every walk, so it clears what an earlier walk reached.
    mutating func record(_ step: SetupState.Step) {
        if step == .inspecting { self = SetupProgress() }
        self.step = step
        guard let stage = step.stage else { return }
        reached.insert(stage)
        last = stage
    }

    func status(of stage: Stage) -> Status {
        switch step {
        case .done:
            return reached.contains(stage) ? .done : .skipped
        case .failed:
            return stage == last ? .failed : passed(stage, before: last)
        default:
            return stage == step.stage ? .current : passed(stage, before: step.stage)
        }
    }

    /// Where the walk stands, counted as Installer counts its steps. A
    /// failure stays on the step that failed.
    var caption: String {
        if step == .done { return "Done" }
        let stage = step.stage ?? last ?? .welcome
        return "Step \(stage.rawValue + 1) of \(Stage.allCases.count)"
    }

    private func passed(_ stage: Stage, before mark: Stage?) -> Status {
        guard let mark, stage.rawValue < mark.rawValue else { return .upcoming }
        return reached.contains(stage) ? .done : .skipped
    }
}
