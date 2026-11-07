import SwiftUI

/// Fits the palette without scrolling. Long instructions scroll inside their editor.
struct PalettePromptForm: View {
    @Binding var draft: PromptDraft
    var isNameFocused: FocusState<Bool>.Binding
    let chooseFile: () -> Void

    var body: some View {
        Form {
            Section {
                TextField("Name", text: $draft.name, prompt: Text("Summarize"))
                    .focused(isNameFocused)
                Picker("Your words", selection: $draft.kind) {
                    ForEach(PromptEdits.kinds, id: \.self) { kind in
                        Text(kind.title).tag(kind)
                    }
                }
                LabeledContent {
                    Button("Choose…", action: chooseFile)
                } label: {
                    Text("File")
                    Text(fileDescription)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            Section("Instructions") {
                TextEditor(text: $draft.instructions)
                    .font(.body)
                    .frame(height: 130)
                    .scrollContentBackground(.hidden)
                    .disabled(!draft.isReadable)
                    .accessibilityLabel("Instructions")
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
    }

    private var fileDescription: String {
        guard let file = draft.file else {
            return "New file in ~/.aria/prompts"
        }
        return (file.path as NSString).abbreviatingWithTildeInPath
    }
}
