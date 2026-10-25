import SwiftUI

struct DonePage: View {
    @ObservedObject var state: SetupState
    let dictationKey: HotkeyKey
    let askHotkey: Chord?

    var body: some View {
        Form {
            if state.selections.hotkey || state.selections.wake || askHotkey != nil {
                Section("How to use Aria") {
                    if state.selections.hotkey {
                        LabeledContent(
                            "Dictate",
                            value: state.hotkeyReady
                                ? "Tap \(dictationKey.title)" : "Works once Accessibility access is given"
                        )
                    }
                    if state.selections.wake {
                        LabeledContent("Dictate hands-free", value: "Say the wake word")
                    }
                    if let askHotkey {
                        LabeledContent("Ask your agent") { ChordLabel(chord: askHotkey) }
                    }
                }
            }
            if let summary = state.summary {
                Section {
                    ForEach(summary.models, id: \.model) { model in
                        LabeledContent(model.model, value: Self.outcome(model.outcome))
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
