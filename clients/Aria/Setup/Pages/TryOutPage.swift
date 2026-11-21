import SwiftUI

struct TryOutPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    @FocusState private var isFieldFocused: Bool

    var body: some View {
        Form {
            Section {
                AriaKeySpot(key: ariaKey).setupPicture()
                if state.isHotkeyReady {
                    TextField(
                        "Dictation", text: $state.typed, prompt: Text("What you say shows up here."), axis: .vertical
                    )
                    .labelsHidden()
                    .lineLimit(3...)
                    .focused($isFieldFocused)
                    // The daemon types where focus is, so the field takes it
                    // whenever it shows, even in a window that already has focus.
                    .onAppear { isFieldFocused = true }
                    if let count = state.typedWordCount {
                        Label {
                            Text(Self.typed(count))
                        } icon: {
                            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                        }
                        .transition(.opacity)
                    }
                } else {
                    AriaKeyAccessNote(keyState: state.ariaKeyState)
                }
            } footer: {
                if let enrolled = state.enrolled {
                    Text(enrolled)
                }
                NoticeText(notice: state.notice)
            }
        }
        .animation(.default, value: state.typedWordCount)
        // Focus stays in the field, so VoiceOver hears the count instead of finding it.
        .onChange(of: state.typedWordCount) {
            if let count = state.typedWordCount {
                NSAccessibility.announce(String(Self.typed(count).characters))
            }
        }
    }

    private static func typed(_ count: Int) -> AttributedString {
        AttributedString(localized: "Aria typed ^[\(count) word](inflect: true).")
    }
}
