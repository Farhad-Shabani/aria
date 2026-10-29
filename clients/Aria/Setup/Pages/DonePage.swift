import SwiftUI

struct DonePage: View {
    @ObservedObject var state: SetupState
    let ariaKey: HotkeyKey

    var body: some View {
        Form {
            if state.selections.hotkey || state.selections.wake {
                Section("How to use Aria") {
                    if state.selections.hotkey {
                        if state.isHotkeyReady {
                            LabeledContent("Dictate", value: "Tap \(ariaKey.title)")
                            LabeledContent("Ask your agent", value: ariaKey.gesture(for: .ask))
                        } else {
                            LabeledContent("Dictate", value: "Works once Accessibility access is given")
                        }
                    }
                    if state.selections.wake {
                        LabeledContent("Dictate hands-free", value: "Say the wake word")
                    }
                }
            }
            if let summary = state.summary {
                Section {
                    ForEach(summary.models, id: \.modelID) { model in
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
