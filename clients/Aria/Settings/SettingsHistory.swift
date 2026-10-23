struct SettingsHistory {
    private(set) var current: SettingsPane
    private var backward: [SettingsPane] = []
    private var forward: [SettingsPane] = []

    init(current: SettingsPane) {
        self.current = current
    }

    var canGoBack: Bool { !backward.isEmpty }
    var canGoForward: Bool { !forward.isEmpty }

    mutating func open(_ pane: SettingsPane) {
        guard pane != current else { return }
        backward.append(current)
        forward.removeAll()
        current = pane
    }

    mutating func goBack() {
        guard let previous = backward.popLast() else { return }
        forward.append(current)
        current = previous
    }

    mutating func goForward() {
        guard let next = forward.popLast() else { return }
        backward.append(current)
        current = next
    }
}
