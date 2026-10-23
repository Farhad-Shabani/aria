import SwiftUI

struct SettingsSidebar: View {
    @Binding var selection: SettingsPane
    let daemon: DaemonState

    var body: some View {
        List(selection: listSelection) {
            Section {
                DaemonStatusCard(state: daemon)
                    .padding(.vertical, 4)
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
