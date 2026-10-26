import SwiftUI

struct DonePage: View {
    @ObservedObject var state: SetupState
    let dictationKey: HotkeyKey
    let askHotkey: Chord?

    var body: some View {
        Form {
            if state.selections.hotkeyEnabled || state.selections.wakeEnabled || askHotkey != nil {
                Section("How to use Aria") {
                    if state.selections.hotkeyEnabled {
                        LabeledContent(
                            "Dictate",
                            value: state.isHotkeyReady
                                ? "Tap \(dictationKey.title)" : "Works once Accessibility access is given"
                        )
                    }
                    if state.selections.wakeEnabled {
                        LabeledContent("Dictate hands-free", value: "Say the wake word")
                    }
                    if let askHotkey {
                        LabeledContent("Ask your agent") { ChordLabel(chord: askHotkey) }
                    }
                }
            }
            if let summary = state.summary {
                Section {
                    ForEach(summary.modelOutcomes, id: \.modelID) { model in
                        LabeledContent(state.title(ofModel: model.modelID), value: Self.outcome(model.outcome))
                    }
                    LabeledContent("Background service", value: "Registered")
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
