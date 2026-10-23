enum SettingsPage: Hashable {
    case pane(SettingsPane)
    /// One item's page inside a pane's list, by the item's id.
    case item(SettingsPane, id: String)

    /// What the sidebar shows selected.
    var pane: SettingsPane {
        switch self {
        case .pane(let pane), .item(let pane, _): pane
        }
    }
}
