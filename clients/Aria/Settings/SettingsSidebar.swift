import SwiftUI

struct SettingsSidebar: View {
    @Binding var selection: SettingsPane
    let daemon: DaemonState
    let items: [SetupItem]

    var body: some View {
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
