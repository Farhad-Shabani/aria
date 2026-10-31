import SwiftUI

struct DonePage: View {
    @ObservedObject var state: SetupState
    let ariaKey: HotkeyKey
    /// Nil when the ask holds a chord or nothing.
    let askCombo: Aria_Daemon_V1_HotkeyCombo?

    var body: some View {
        Form {
            if state.selections.hotkey {
                Section("How to Use Aria") {
                    if state.isHotkeyReady {
                        LabeledContent("Dictate", value: "Tap \(ariaKey.title)")
                        if let askCombo {
                            LabeledContent("Ask your assistant", value: ariaKey.gesture(for: askCombo))
                        }
                    } else {
                        LabeledContent("Dictate", value: "Needs permission")
                    }
                }
            }
            if let summary = state.summary {
                Section {
                    ForEach(summary.models, id: \.modelID) { model in
                        LabeledContent(state.title(ofModel: model.modelID), value: Self.outcome(model.outcome))
                    }
                    LabeledContent("Background service", value: "On")
                    if let heard = state.heard {
                        LabeledContent("Heard", value: heard)
                    }
                } footer: {
                    NoticeText(notice: state.notice)
                }
            }
        }
    }

    private static func outcome(_ outcome: Aria_Setup_V1_Outcome) -> String {
        switch outcome {
        case .downloaded: "Downloaded"
        case .present: "Already there"
        case .skipped: "Skipped"
        case .unspecified, .UNRECOGNIZED: "Unknown"
        }
    }
}
