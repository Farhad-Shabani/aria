import SwiftUI

struct GrantPage: View {
    let ariaKey: AriaKeyState

    var body: some View {
        Form {
            if ariaKey == .refused {
                Section {
                    Text(
                        "macOS didn’t let Aria hear the Aria key, though access is on. Run `aria doctor` in Terminal to see why."
                    )
                }
            } else {
                Section {
                    LabeledContent {
                        ProgressLabel("Waiting")
                    } label: {
                        Text("Privacy & Security > \(ariaKey.privacyPane)")
                        Text(detail)
                    }
                } header: {
                    // A header has no box, so the window drawing stands on the page like a screenshot.
                    AccessPicture(pane: ariaKey.privacyPane).setupPicture()
                } footer: {
                    Text("Setup continues when Aria is turned on.")
                }
            }
        }
    }

    private var detail: String {
        ariaKey == .needsInputMonitoring
            ? "Lets Aria hear the Aria key." : "Lets Aria hear the Aria key and type for you."
    }
}
