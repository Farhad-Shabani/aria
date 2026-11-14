import SwiftUI

struct SettingsSidebar: View {
    @Binding var selection: SettingsPane
    let daemon: DaemonState
    let items: [SetupItem]
    let reveal: (SettingsRow) -> Void
    @Binding var focusRequest: SettingsRow?
    @State private var query = ""

    var body: some View {
        Group {
            if let results = SettingsSearch.results(for: query) {
                SettingsSearchResults(
                    query: query, results: results, selection: $selection, reveal: reveal,
                    focusRequest: $focusRequest)
            } else {
                panes
            }
        }
        .searchable(text: $query, placement: .sidebar, prompt: "Search")
        // A request whose row waits for the background service could take
        // focus minutes later, away from the search field.
        .onChange(of: query) { focusRequest = nil }
    }

    private var panes: some View {
        List(selection: listSelection) {
            Section {
                SidebarHeader { DaemonStatusLabel(state: daemon) }
            }
            if !items.isEmpty {
                Section {
                    HStack {
                        Label {
                            Text(SettingsPane.finishSetup.title)
                        } icon: {
                            SettingsIcon(pane: .finishSetup)
                        }
                        Spacer()
                        CountBadge(count: items.count)
                    }
                    .accessibilityElement(children: .combine)
                    .tag(SettingsPane.finishSetup)
                }
            }
            ForEach(SettingsPane.sidebarGroups, id: \.self) { group in
                Section {
                    ForEach(group) { pane in
                        Label {
                            Text(pane.title)
                        } icon: {
                            SettingsIcon(pane: pane)
                        }
                    }
                }
            }
        }
    }

    /// A list can always deselect, but the window always shows a pane.
    private var listSelection: Binding<SettingsPane?> {
        Binding(
            get: { selection },
            set: { pane in
                if let pane { selection = pane }
            }
        )
    }
}

/// The sidebar while a search is typed: each matching pane, with its
/// matching rows beneath it, as System Settings lists them.
private struct SettingsSearchResults: View {
    private enum Pick: Hashable {
        case pane(SettingsPane)
        case row(SettingsRow)
    }

    let query: String
    let results: [SettingsSearch.Result]
    @Binding var selection: SettingsPane
    let reveal: (SettingsRow) -> Void
    @Binding var focusRequest: SettingsRow?
    @State private var picked: Pick?

    var body: some View {
        List(selection: Binding(get: { picked }, set: pick)) {
            ForEach(results, id: \.pane) { result in
                Section {
                    Label {
                        Text(result.pane.title)
                    } icon: {
                        SettingsIcon(pane: result.pane)
                    }
                    .tag(Pick.pane(result.pane))
                    ForEach(result.rows) { row in
                        Label {
                            Text(row.title)
                        } icon: {
                            // Lines the title up under its pane's.
                            Color.clear.frame(width: SettingsMetrics.iconSide, height: SettingsMetrics.iconSide)
                        }
                        .tag(Pick.row(row))
                    }
                }
            }
        }
        .overlay {
            if results.isEmpty {
                ContentUnavailableView.search(text: query)
            }
        }
        // Arrows show each row. Return focuses the setting, so the keyboard
        // can change it. Return also marks the row again.
        .onKeyPress(.return) {
            guard case .row(let row) = picked else { return .ignored }
            reveal(row)
            focusRequest = row
            return .handled
        }
        .onChange(of: query) { picked = nil }
    }

    private func pick(_ pick: Pick?) {
        picked = pick
        focusRequest = nil
        switch pick {
        case .pane(let pane): selection = pane
        case .row(let row): reveal(row)
        case nil: break
        }
    }
}
