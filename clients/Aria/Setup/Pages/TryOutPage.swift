import SwiftUI

struct TryOutPage: View {
    @ObservedObject var state: SetupState
    let ariaKey: AriaKey
    @FocusState private var isFieldFocused: Bool

    var body: some View {
        Form {
            Section {
                AriaKeyCap(key: ariaKey)
                    .frame(maxWidth: .infinity)
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

/// The Aria key drawn as a key on a keyboard, so the person knows which one to tap.
private struct AriaKeyCap: View {
    let key: AriaKey

    var body: some View {
        VStack(spacing: 4) {
            Image(systemName: symbol)
                .font(.title)
            Text(key.title)
                .font(.callout)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .background(.quaternary, in: RoundedRectangle(cornerRadius: 10))
        .overlay {
            RoundedRectangle(cornerRadius: 10).strokeBorder(.tertiary)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Aria key, \(key.title)")
    }

    private var symbol: String {
        switch key {
        case .rightOption: "option"
        case .fn: "fn"
        }
    }
}
