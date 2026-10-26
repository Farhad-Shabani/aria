struct SettingsHistory {
    private(set) var current: SettingsPage
    private var backward: [SettingsPage] = []
    private var forward: [SettingsPage] = []
    private var selectedItems: [SettingsPane: String] = [:]

    init(current: SettingsPage) {
        self.current = current
    }

    var canGoBack: Bool { !backward.isEmpty }
    var canGoForward: Bool { !forward.isEmpty }

    func selectedItem(in pane: SettingsPane) -> String? {
        selectedItems[pane]
    }

    mutating func selectItem(_ id: String?, in pane: SettingsPane) {
        selectedItems[pane] = id
    }

    mutating func open(_ page: SettingsPage) {
        guard page != current else { return }
        if case .item(let pane, let id) = page {
            selectItem(id, in: pane)
        }
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
