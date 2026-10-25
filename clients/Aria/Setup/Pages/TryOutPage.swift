import SwiftUI

struct TryOutPage: View {
    private static let transcriptHeight: CGFloat = 72

    @ObservedObject var state: SetupState

    var body: some View {
        Form {
            Section {
                transcript
                    .frame(maxWidth: .infinity, minHeight: Self.transcriptHeight, alignment: .topLeading)
            } footer: {
                if let enrolled = state.enrolled {
                    Text(enrolled)
                }
                NoticeText(notice: state.notice)
            }
        }
    }

    @ViewBuilder private var transcript: some View {
        if let heard = state.heard {
            Text(heard)
                .font(.title3)
                .textSelection(.enabled)
        } else if state.isBusy {
            Label {
                Text("Listening…")
            } icon: {
                ProgressView().controlSize(.small)
            }
            .foregroundStyle(.secondary)
        } else {
            Text("What you say shows up here.").foregroundStyle(.tertiary)
        }
    }
}
