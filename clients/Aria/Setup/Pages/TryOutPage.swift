import SwiftUI

struct TryOutPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    @FocusState private var isFieldFocused: Bool

    var body: some View {
        Form {
            Section {
                VStack(spacing: 8) {
                    AriaKeySpot(key: ariaKey)
                    Text(ariaKey.title)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                }
                .padding(.vertical, 8)
                if state.isHotkeyReady {
                    TextField(
                        "Dictation", text: $state.typed, prompt: Text("What you say shows up here."), axis: .vertical
                    )
                    .labelsHidden()
                    .lineLimit(3...)
                    .focused($isFieldFocused)
                } else {
                    Text("The Aria key needs permission. Allow it in Settings later.")
                        .foregroundStyle(.secondary)
                }
            } footer: {
                if let enrolled = state.enrolled {
                    Text(enrolled)
                }
                NoticeText(notice: state.notice)
            }
        }
        // The daemon types where focus is, so the field must hold it.
        .onAppear { isFieldFocused = true }
    }
}
