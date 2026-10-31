struct SetupProgress {
    /// The rows of the setup rail. Done and failure sit outside them.
    enum Stage: Int, CaseIterable {
        case welcome
        case engines
        case install
        case hotkey
        case voice
        case tryOut

        var title: String {
            switch self {
            case .welcome: "Welcome"
            case .engines: "Engines"
            case .install: "Install"
            case .hotkey: "Permission"
            case .voice: "Your Voice"
            case .tryOut: "Try It"
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

    /// The share of stages behind the walk. A failure stops it where it failed.
    var fraction: Double {
        if step == .done { return 1 }
        let reachedSoFar = (step.stage ?? last)?.rawValue ?? 0
        return Double(reachedSoFar) / Double(Stage.allCases.count)
    }

    private func passed(_ stage: Stage, before mark: Stage?) -> Status {
        guard let mark, stage.rawValue < mark.rawValue else { return .upcoming }
        return reached.contains(stage) ? .done : .skipped
    }
}
