import SwiftUI

struct PromptResultView: View {
    let result: PromptResult
    let copy: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Label(result.promptName, systemImage: "text.bubble")
                    .font(.callout.weight(.semibold))
                Text(HistoryDay.time(of: result.created))
                    .font(.caption)
                Spacer(minLength: 0)
                Button("Copy", systemImage: "doc.on.doc", action: copy)
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Copy This Result")
            }
            .foregroundStyle(.secondary)
            Text(result.text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(result.promptName) result")
    }
}
