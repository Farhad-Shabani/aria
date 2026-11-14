import Foundation

enum SettingsSearch {
    struct Result: Equatable {
        let pane: SettingsPane
        let rows: [SettingsRow]
    }

    /// Nil for a blank query. A pane whose own title matches is listed
    /// even when none of its rows do.
    static func results(for query: String) -> [Result]? {
        let needle = query.trimmingCharacters(in: .whitespaces)
        guard !needle.isEmpty else { return nil }
        return SettingsPane.sidebarGroups.flatMap { $0 }.compactMap { pane in
            let rows = SettingsRow.allCases.filter { $0.pane == pane && matches($0, needle) }
            guard !rows.isEmpty || pane.title.localizedStandardContains(needle) else { return nil }
            return Result(pane: pane, rows: rows)
        }
    }

    private static func matches(_ row: SettingsRow, _ needle: String) -> Bool {
        ([row.title] + row.keywords).contains { $0.localizedStandardContains(needle) }
    }
}
