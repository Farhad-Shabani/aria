import SwiftUI

struct SettingsSidebar: View {
    @Environment(\.openWindow) private var openWindow
    @Binding var selection: SettingsPane
    let daemon: DaemonState
    @ObservedObject var setup: SetupStatus

    var body: some View {
        List(selection: listSelection) {
            Section {
                SidebarHeader { DaemonStatusLabel(state: daemon) }
            }
            if !setup.required.isEmpty {
                Section {
                    Button {
                        openWindow(id: WindowID.setup)
                    } label: {
                        Label {
                            Text("Finish Setting Up Aria")
                        } icon: {
                            SettingsIcon(symbol: "exclamationmark.circle.fill", isWarning: true)
                        }
                    }
                    .buttonStyle(.plain)
                    .badge(setup.required.count)
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
