struct SettingsHistory {
    private(set) var current: SettingsPage
    private var backward: [SettingsPage] = []
    private var forward: [SettingsPage] = []

    init(current: SettingsPage) {
        self.current = current
    }

    var canGoBack: Bool { !backward.isEmpty }
    var canGoForward: Bool { !forward.isEmpty }

    /// The item whose page Back just left, so its list can mark it.
    func lastItem(in pane: SettingsPane) -> String? {
        guard case .item(pane, let id) = forward.last else { return nil }
        return id
    }

    mutating func open(_ page: SettingsPage) {
        guard page != current else { return }
        backward.append(current)
        forward.removeAll()
        current = page
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
